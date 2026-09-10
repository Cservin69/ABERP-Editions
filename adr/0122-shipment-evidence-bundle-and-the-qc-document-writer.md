# ADR-0122 — The shipment evidence bundle: a writer for the `qc/` entry, on the scope that is actually joinable (D-99 residual 1)

- **Status:** **Accepted** (2026-09-10) — passed adversarial review round 1
  after a FIX-FIRST verdict: three blocking defects (a silent drop of the whole
  `export.*` family, a manifest-version bump that broke every archive already
  written, and a refusal that fired on a routine renderer upgrade) are designed
  out below, and four properties were pressed and held. Safe to build, in the
  slices at the end.
- **Date:** 2026-09-10
- **Deciders:** Design pass + adversarial review round 1, 2026-09-10
  (`docs/_adversarial-adr-0122-shipment-bundle-round1.md`).
- **Related:** ADR-0029 (the invoice evidence bundle: shape, membership rule,
  refusal posture), ADR-0030 (the mirror-agreement assertion the export makes),
  ADR-0199 §D7 (the QC report's hash pin: *the hash is pinned, the bytes are
  not stored*), ADR-0064 §6 (the dispatch kinds), ADR-0094 (the
  no-new-`EventKind` blast-radius clause), D-99 residual 1 (AC10: "`qc_archive_path`
  has zero non-test callers, so no export ever emits a `qc/` file for the
  verifier to check").

## Context

### The gap

`aberp-verify` can already check a QC report inside an evidence bundle. It
accepts a `qc/` directory in the bundle allow-list
(`crates/aberp-verify/src/bundle.rs:134`), keys the files by their `qc/…`
portion, re-hashes each one and compares it against the `rendered_sha256`
pinned on the matching `qcr.report_issued` chain entry
(`check_qc_report_pins`, `crates/aberp-verify/src/verify.rs:676`).

**Nothing produces such a file.** `qc_archive_path`
(`crates/aberp-verify/src/bundle.rs:353`) has exactly two callers: the
verifier that consumes it, and its own unit test. The one bundle writer in the
tree — `apps/aberp/src/export_invoice_bundle.rs` — packs
`manifest.json`, `chain.jsonl` and `nav/*` and knows nothing about QC
(`pack_bundle`, `:1121`).

So the retention half of AC10 is wired and the production half is absent. An
auditor asking for the inspection record that backs a shipment gets nothing
from the export; the report is reachable only through the running application
at `GET /api/qc-reports/:id`.

### The join the backlog assumed does not exist

D-99 residual 1 states the work as "the invoice→dispatch→WO→report join that
decides which reports belong in an invoice-scoped slice." **That join is
severed at its first hop, and the severance is in shipped code.**

The last two hops are sound and ledger-derivable:

- `qcr.report_attached_to_shipment` carries `qcr_id` **and** `dsp_id` **and**
  `wo_id` (`crates/aberp-qa/src/qc/reports.rs:1208`). It is emitted by
  `bind_reports_to_dispatch`, which runs inside `mark_shipped`'s single
  transaction, so a bound report and a shipped dispatch commit or roll back
  together (ADR-0199 §D6).
- `qcr.report_issued` carries `qcr_id` and `rendered_sha256`
  (`crates/aberp-qa/src/qc/reports.rs:1151`).

The first hop is the problem. Walk it as the code actually runs:

1. `mark_shipped` fires `mes.dispatch_shipped` with
   `spawned_invoice_id: Option<String>` (`crates/aberp-dispatch/src/audit.rs:89`).
2. The value in that field is **not an invoice id**. The production spawner is
   `BillingInvoiceSpawner`, and it returns a `drf_<ULID>` — a Stage-1 invoice
   *draft* (`apps/aberp/src/invoice_draft.rs:658`). Its own doc comment says so:
   "the value is in fact 'spawned-invoice-or-draft id'."
3. Promotion of that draft to a real `inv_*` invoice is **a form-fill, not a
   transaction**. The route comment is explicit: "the SPA pre-fills the form
   from a GET to `/api/invoice-drafts/:id`, then DELETEs the draft after the
   invoice creation succeeds; cross-pipeline atomic promote is deferred to a
   future PR" (`apps/aberp/src/serve.rs:5188`).
4. `issue_invoice` therefore records **no** provenance: it fires
   `InvoiceSequenceReserved` + `InvoiceDraftCreated` against a freshly minted
   `inv_*` and never names the `drf_*` the operator copied the numbers from.
5. The delete that follows NULLs the pointer that might otherwise have survived
   in the row: `delete_draft_in_tx` calls `null_spawned_invoice_id_in_tx`
   (`apps/aberp/src/invoice_draft.rs:555`).

After an ordinary ship→invoice flow the ledger holds `dsp_X`, `drf_Y` and
`inv_A`, an edge `dsp_X ↔ drf_Y`, an edge nothing draws between `drf_Y` and
`inv_A`, and a deleted draft row. **`inv_A → dsp_X` is not derivable from the
ledger, and not derivable from the tables either.**

The invoice exporter already knows this and says it out loud in an
exhaustiveness arm, in the course of explaining why the dispatch kinds are
excluded from an invoice bundle: "The `spawned_invoice_id` field on
`DispatchShipped` points AT a Stage 1 invoice draft"
(`apps/aberp/src/export_invoice_bundle.rs:722`). The observation is recorded;
its consequence for evidence scope is not.

This is **finding F1** below. It is larger than this ADR and is deliberately
*not* fixed here.

### Why the scope has to move, not the join

Given F1, an invoice-scoped QC slice can only be built on one of three things:

- a join computed from data that does not exist (impossible);
- an operator-supplied `--dispatch-id` alongside `--invoice-id`, i.e. asking
  the human to assert the link the code failed to record — the
  trust-the-operator-not-the-code inversion this codebase refuses everywhere
  else; or
- closing F1 first, which means building the atomic promote that PR-230b
  deferred.

None of those is a small change, and the third is a money-path change.

But the QC report is not attached to an invoice in the first place. It is
attached to a **shipment**: `bind_reports_to_dispatch(tx, ctx, wo_id, dsp_id)`,
inside `mark_shipped`. The document set that answers "what left the building,
and was it conforming?" is dispatch-scoped by construction. Scoping the
evidence export the same way is not a workaround for F1 — it is the scope the
data already has.

## Decision

**Add a second evidence bundle, scoped to a dispatch, and put the `qc/` writer
there.** A new subcommand `aberp export-shipment-bundle --dispatch-id dsp_X`,
producing the same `.tar.zst` shape the invoice bundle produces, with a `qc/`
directory the existing verifier already knows how to check.

The invoice bundle is **not modified**. No change to `BundleMembershipProbe`,
to `filter_invoice_slice`, to the invoice manifest, or to any money path.

### D1 — Membership: a two-pass rule, named as such

The invoice bundle's membership rule (ADR-0029 §2) is *flat*: a payload-local,
order-independent "any id-shaped field equals the target" predicate. The
shipment bundle cannot be flat — the report entries do not carry `dsp_id` on
both hops — so its rule is stated as two explicit passes rather than smuggled
into a widened probe:

- **Pass 1 (flat, over a NAMED FIELD SET).** Every entry whose payload carries
  the target dispatch id in any of three field names:

  | field | payloads that use it |
  |---|---|
  | `dsp_id` | `mes.dispatch_created`, `mes.dispatch_shipped`, `qcr.report_attached_to_shipment` |
  | `shipment_id` | `export.shipment_logged` (`ExportShipmentLoggedPayload`, `crates/aberp-dispatch/src/audit.rs:217`) |
  | `entity_id` (+ `entity_kind == "dispatch"`) | `export.access_check` only (`crates/aberp-dispatch/src/audit.rs:165`; `entity_kind: "dispatch"` at `apps/aberp/src/serve.rs:19650`) |

  > **Correction, found while building slice 2.** Round 1's version of this
  > table also listed `export.classification_set` on the `entity_id` row,
  > because it uses the same two fields. That is **wrong**: its firing site
  > writes `entity_kind: "product"` with the WO's `product_id`
  > (`crates/aberp-dispatch/src/repository.rs:698`). It is a determination
  > about a **commodity**, not about this shipment, and its id is a `prd_*`
  > that no dispatch-keyed rule can match.
  >
  > It stays **out** of the slice, and that is the right answer rather than a
  > shortfall. Sweeping it in would need a third declared hop
  > (dispatch → WO → product), and that hop would pull **every other
  > shipment's** classification rows for the same product into a per-shipment
  > bundle. The per-shipment fact is already carried where it belongs:
  > `export.shipment_logged.ecn_or_authorization` is "populated from the same
  > determination the `export.classification_set` row carries"
  > (`audit.rs:228`), scoped to this shipment, and it **is** in the slice.
  >
  > Pinned both ways:
  > `the_export_family_is_in_the_slice_by_its_own_field_names` and
  > `a_product_scoped_classification_row_is_not_in_a_shipment_slice`.

  **The field set is load-bearing and hand-listed**, exactly like
  `BundleMembershipProbe`'s (`apps/aberp/src/export_invoice_bundle.rs:147`): a
  future payload that names a dispatch by a fourth field name must extend it in
  the same PR. Round 1 found the single-field version of this rule silently
  dropped **all three** `export.*` kinds — the denied-party screening decision,
  the ECCN determination, and the export shipment record — from a defense
  evidence bundle, with nothing in the archive saying so.

  Plain equality on `entity_id` is sound **because ids are prefix-namespaced**
  (`dsp_<ULID>` can never collide with `prd_`/`wo_`); `entity_kind == "dispatch"`
  is read anyway as belt-and-braces. A later contributor who drops the
  discriminant should know the prefix is what they are then leaning on.

  A caution the sibling earns: `probe_field_set_covers_every_payload_id_field`
  (`export_invoice_bundle.rs:1378`) is a **hand-listed** test and it has already
  missed one — `DispatchShippedPayload::spawned_invoice_id` is an
  invoice-id-shaped field it never listed. A hand-listed pin does not catch a
  field nobody thought to hand-list. Pin the field set by round-tripping each
  *real payload struct* through the probe, not by re-listing strings.
- **Pass 2 (transitive, exactly one hop, over the ids pass 1 found).** Every
  entry whose payload carries a `qcr_id` in the set of `qcr_id`s pass 1
  collected. This sweeps `qcr.report_issued` — which is what the verifier needs
  in `chain.jsonl` to read `rendered_sha256` at all — and the rest of the
  report's own lifecycle (`qcr.report_frozen`, `qcr.report_rendered`,
  `qcr.report_voided`, …).

**The hop count is fixed at one, and closure is not taken.** A transitive rule
that chases every id it meets walks the whole ledger; "one declared hop, over a
named field" is auditable and terminates. If a later slice needs the WO's part
marks, that is a second *declared* hop, added deliberately with its own test —
not a closure that silently grew.

**Empty ids never enter the set.** `BundleMembershipProbe::matches` refuses an
empty *target* as defence-in-depth (`export_invoice_bundle.rs:180`); pass 2
takes the hazard from the other side, because it builds its id set **from
payloads**. One payload carrying `"qcr_id": ""` would otherwise put the empty
string in the set and sweep every entry with an empty `qcr_id` into the slice.
Empties are filtered on *insert*, not only on compare.

Both passes run over the full entry list and the union is re-sorted by `seq`,
so `chain.jsonl` stays in chain order regardless of which pass found a row.

**Termination is a property of the field names, not just of policy.** Exactly
five payload sites carry `qcr_id` (`crates/aberp-qa/src/qc/reports.rs:1015`,
`:1130`, `:1205`, `:1239`, `:1281` — frozen, issued, attached, rendered,
voided). The one field that could grow the set, `superseded_by_qcr_id` on the
void payload (`:1284`), has a **different name**, so a one-hop rule keyed on
`qcr_id` cannot chase it even by accident.

### D1b — a dispatch that never shipped is REFUSED, not bundled

Pass 1 matches `mes.dispatch_created`, so a Drafted or cancelled dispatch yields
a one-entry slice — not empty, so ADR-0029 §2's zero-entry loud-fail does not
catch it. The operator would receive an archive named for a shipment that never
happened, containing no reports and no export decisions, **indistinguishable in
shape from a shipment whose documents were dropped**.

So the command refuses unless the slice contains a `mes.dispatch_shipped` for
the target, and the message names what the dispatch's ledger actually shows
(created-only, cancelled, unknown id). See Open questions Q4: this deliberately
leaves a cancelled dispatch with no export path.

### D2 — Bytes: re-rendered, and a mismatch is triaged by `renderer_version`

ADR-0199 §D7 decided the report's bytes are not stored: the hash is pinned, the
document is derivable, and `aberp-qc-pdf` is a pure renderer (no clock, no I/O,
no RNG) precisely so that re-rendering reproduces them.

So the exporter re-renders. `apps/aberp/src/qc_report.rs:541`'s `render_report`
already returns exactly the tuple this needs —
`(report, bytes, sha, matches: Option<bool>)` — where `matches` is the
comparison against the pinned `rendered_sha256`.

**A bare "refuse on mismatch" is wrong, and round 1 caught it.** `matches` is a
SHA comparison and nothing else, so *any* change to `aberp-qc-pdf`'s layout
changes every byte of every report ever issued. Refusing on that would make the
first export after any renderer deploy fail for every dispatch, forever, with no
operator remedy — an outage in the tool an auditor is waiting on, announced with
a message that says "tampered" about a routine upgrade.

The discriminator already exists, and the codebase built it for exactly this
question. `crates/aberp-qc-pdf/Cargo.toml:4-10` deliberately pins the crate's
own version rather than inheriting the workspace's `0.0.0`:

> every issued report recorded `renderer_version = "0.0.0"` — which made the
> field useless for the one question it exists to answer: when a re-render's SHA
> does not match the pin, was the RENDERER changed or were the rows TAMPERED
> with? … **Bump it in the same commit as any change to the rendered bytes.**

`renderer_version` is on the row and on the `qcr.report_issued` payload
(`crates/aberp-qa/src/qc/reports.rs:1152`), and it prints into the page footer,
so a bump necessarily changes the bytes. The verdict is therefore a **2×2**, on
the stored `renderer_version` versus `aberp_qc_pdf::QC_PDF_RENDERER_VERSION`:

| stored version | SHA | verdict |
|---|---|---|
| equal | equal | **bundle it** — the ordinary path |
| **equal** | **differs** | **REFUSE the whole export.** Same renderer, different bytes: the frozen rows moved under a report that is supposed to be frozen. This is the tamper signal and it earns the loud stop; the message names the report and both hashes. |
| differs | differs | **OMIT and NAME it** via D3, `reason: "renderer_version <stored> is no longer available (current <current>) — the issued bytes cannot be reproduced"`. The bundle is still produced and the chain entry with the original SHA is still in `chain.jsonl`. |
| differs | equal | Cannot occur (the version prints into the page). Treated as the ordinary path — an assertion here buys nothing and could only turn a harmless surprise into an abort. |

Refusing on the tamper row is the house posture for the surrounding code: the
invoice exporter already refuses on a failed `verify_chain` (ADR-0029 §6) and on
mirror divergence (ADR-0030 §5). Omitting-and-naming on the renderer row is the
same posture applied to a condition the operator cannot clear: a bundle that
tells you which document is missing and why beats both a refusal and a silent
hole. See **F2** for the retention consequence this branch exposes.

### D3 — A report that is no longer current is omitted, and the omission is NAMED

`render_report` refuses a `Voided` or `Superseded` report outright
(`QcReportError::NotCurrent`) — ADR-0199's round-3 decision, on the grounds
that its unmarked PDF would read as a valid certificate. That refusal stands
and is not relitigated here.

The consequence for a bundle is that a report bound to this dispatch can be
legitimately unrenderable. The verifier tolerates this — "an issued report with
no bundled PDF is NOT a failure" (`verify.rs:674`) — but an auditor must not
have to infer it. So the manifest carries an explicit
`qc_documents_omitted: [{ qcr_id, report_number, reason }]` block, and the
operator-visible summary line prints the count. A void is evidence; hiding the
hole in the document set is not.

### D4 — One opener: `Ledger::into_connection`

The command needs the ledger entries (for the join and `chain.jsonl`) **and** a
`&Connection` to the same DB (for `render_report`, which reads `qc_reports` +
`qc_report_lines`).

It cannot open a second connection: two `Connection::open` calls on one DuckDB
file are two database instances contending for the same file lock, and the
opener census (CHECK 10h / 10i) would red a new one regardless.

`Ledger` owns its `Connection` privately (`crates/audit-ledger/src/storage/mod.rs:140`)
and exposes no accessor. Add a **consuming** one:

```rust
pub fn into_connection(self) -> Connection
```

Consuming, not borrowing, is the load-bearing part. A `fn conn(&self) -> &Connection`
would be a laundering channel of exactly the shape CHECK 10P classifies and
`from_connection` was fenced for (PR #34): it hands out a writer with the
`Ledger`'s provenance stripped. `into_connection` destroys the `Ledger`, so
there is no interval in which both a `Ledger` and a bare `Connection` on the
same file are live in one scope. The export's order is:
open → `verify_chain` → `entries` → join → **consume** → render → pack.

### D5 — Manifest: version 2, and a verifier that accepts a SET

The verifier's `Manifest` requires `invoice_id: String`
(`crates/aberp-verify/src/bundle.rs:179`). A shipment bundle has no invoice id
to put there, and putting the dispatch id in a field called `invoice_id` is a
lie in the artifact whose whole purpose is not to lie.

`MANIFEST_VERSION` goes to **2**, adding:

- `scope_kind: "invoice" | "dispatch"` — required from v2 on, and the discriminant;
- `scope_id: String` — the target id;
- `invoice_id: Option<String>` — retained and still populated for
  `scope_kind: "invoice"`, so an existing reader keyed on it keeps working;
- `qc_documents: u64` and `qc_documents_omitted: [...]` (D3).

**The verifier must accept `{1, 2}`, not switch from 1 to 2.** Round 1 found the
first draft of this reasoned about one direction only. The verifier does not
range-check, it equality-checks —
`if manifest.version == SUPPORTED_MANIFEST_VERSION`
(`crates/aberp-verify/src/verify.rs:114`, constant at `bundle.rs:49`) — so both
directions break:

- bump the writer alone → **every** bundle, invoice and shipment alike, fails
  the version check on the current verifier;
- bump `SUPPORTED_MANIFEST_VERSION` to 2 as well → **every v1 archive already
  written** fails on the only verifier that exists. Those archives are the
  artifact whose entire purpose is to still verify years later. Breaking them to
  add a field is not a trade this ADR is entitled to make.

So the constant becomes a **known-versions set**, and the reader normalises: a
v1 manifest has no `scope_kind`/`scope_id` and is read as
`scope_kind: "invoice"`, `scope_id: invoice_id`. The new fields carry
`#[serde(default)]` so a v1 document deserialises; `invoice_id` becomes
`Option<String>` on the struct but a **v1 manifest missing it is malformed and
must fail**, not silently default to `None`. Pinned by a v1 golden and a v2
golden verifying in the same test.

The forward-compatibility note the constant's own doc comment promises ("a newer
`aberp-verify` may understand this bundle") describes a verifier that accepts a
set. This is where it becomes true.

**The invoice exporter emits version 2 too**, with `scope_kind: "invoice"` and
`scope_id == invoice_id`. Two writers emitting two manifest versions would put
the fork in the artifact instead of in the code.

### D6 — Edition: Defense-only, and honest about it on Portable

QC reporting is Defense-gated (`qc_reporting_allowed_for`, ADR-0199), and
`render_report` calls `assert_qc_reporting_allowed` before touching a row. The
subcommand is gated the same way and refuses on Portable with the reason,
rather than emitting a bundle whose `qc/` directory is empty for a reason the
archive does not record.

## Consequences

- An auditor can be handed one file per shipment containing: the chain slice
  covering the dispatch, the export-control decisions fired at the same
  boundary, and the QC report PDFs, each re-hashed against the chain by
  `aberp-verify`. That is AC10.
- The `qc/` allow-list, `qc_archive_path` and `check_qc_report_pins` acquire
  their first producer. They stop being untested-in-anger surface.
- The invoice bundle is untouched, so no money-path artifact changes shape
  beyond the manifest version bump in D5.
- **A shipment bundle is not an invoice bundle.** Neither subsumes the other
  while F1 is open, and an auditor wanting both gets two files. That is the
  honest state of the data, and it is recorded rather than papered over.
- `Ledger` gains one public method. It is consuming, and the ADR names why a
  borrowing version was refused so a later contributor does not "simplify" it.

## Adversarial review

**Round 1 (2026-09-10) — FIX-FIRST, now applied.** Full write-up:
`docs/_adversarial-adr-0122-shipment-bundle-round1.md`. Everything in it was
checked against the tree, not reasoned from this ADR's text.

Three blocking defects, all of which failed **silently or totally** rather than
loudly and partially — the wrong direction for an evidence artifact:

- **B1 — pass 1's single-field probe dropped the whole `export.*` family.** The
  three payloads key on `shipment_id` and `entity_kind`/`entity_id`, never
  `dsp_id`. A defense bundle would have shipped its QC documents without the
  screening decision, the ECCN determination and the export shipment record,
  with nothing saying so. Closed by D1's named field set.
- **B2 — the manifest bump broke every archive that already exists.** The
  verifier equality-checks the version, so bumping the writer breaks new bundles
  on the old verifier *and* bumping the constant breaks every v1 archive on the
  new one. Closed by D5's known-versions set + v1 normalisation.
- **B3 — the refusal fired on a routine renderer upgrade.** `matches` is a SHA
  comparison; a layout change moves every byte of every report ever issued, so
  the first export after any renderer deploy would refuse for every dispatch
  with no operator remedy. Closed by D2's 2×2 on `renderer_version` — the
  discriminator `aberp-qc-pdf` was versioned separately to provide.

Two non-blocking, also applied: **C1** a never-shipped dispatch produced a
one-entry "evidence" bundle (D1b now refuses); **C2** pass 2 needed the
empty-id guard on *insert*, because unlike its sibling it builds its set from
payloads.

One recorded rather than fixed: **F2** below.

Four properties were pressed and held: D4's ordering (`detect_mirror_agreement`
takes a path and already-read entries, so consuming the `Ledger` strands
nothing); pass 2's termination (the five `qcr_id` sites, and `superseded_by_qcr_id`
being a *different field name*, so a one-hop rule cannot chase it); F1 itself
(verified independently at four sites); and D6's edition gate (`render_report`
already asserts first, so the subcommand gate is belt-and-braces).

## Alternatives considered

- **Widen the invoice bundle with a transitive QC pass.** Rejected: the
  transitive walk's first hop does not exist (F1). Every version of this needs
  either the operator to assert the link or the atomic promote to be built
  first.
- **Take `--dispatch-id` as a second argument to `export-invoice-bundle`.**
  Rejected: it makes the operator the source of a provenance fact the code was
  supposed to record, and an incorrect assertion silently binds one shipment's
  QC documents to another shipment's invoice. That is a worse artifact than no
  artifact.
- **Store the rendered PDF bytes at issuance.** Rejected upstream by ADR-0199
  §D7 and not reopened here: it loads every checkpoint, mirror sync and
  snapshot with a derivable payload, and the AP-artifact-on-disk precedent only
  holds because NAV keeps the master copy — nobody keeps a QC report's.
- **Close F1 first (build the atomic promote), then do the invoice-scoped
  slice.** Not rejected — deferred. It is the right eventual shape and it is a
  money-path change that deserves its own ADR and its own adversarial round.
  Doing it as a prerequisite for a document writer would bundle a
  billing-pipeline change behind a compliance feature, which is how a fix
  becomes a regression.

## Open questions

- **Q1 — is the shipment bundle also the right home for the part-mark trace?**
  `wo_part_marks` / `trace_part_uid` is the serialised-unit spine, and it is
  one more declared hop from `wo_id`. Deliberately out of scope for this ADR;
  it wants its own decision about what an auditor is owed per shipment.
- **Q2 — should `export-shipment-bundle` also assert the mirror agreement?**
  The invoice exporter does (ADR-0030 §5). Default answer: yes, same code, same
  refusal. Flagged because it is a refusal the operator will meet.
- **Q3 — one archive per dispatch, or one per WO?** A WO can have several
  dispatches. Default answer: per dispatch, because that is the binding scope.
- **Q4 — a cancelled dispatch has no export path.** D1b refuses anything without
  a `mes.dispatch_shipped`, which is right for an artifact called a *shipment*
  evidence bundle and leaves a cancelled dispatch's trail reachable only through
  the running application. Recorded as a real gap rather than papered over with
  a success. Whether a cancelled dispatch is ever an audit subject is an owner
  question, not a code one.

## Findings recorded, not fixed

### F1 — the outgoing invoice has no provenance back to the shipment it bills

Stated in full in Context. Summary of the mechanism: the dispatch spawns a
`drf_*` draft; promotion is a form-fill the SPA performs; `issue_invoice`
records no `drf_*`; the draft is then deleted and its dispatch pointer NULLed.
The result is an outgoing invoice whose audit trail cannot be connected to the
work order, the dispatch, the part marks, the export-control screening or the
QC reports that justify it.

For a NAV audit this is tolerable — NAV's interest starts at the invoice. For a
**defense** evidence trail it is the seam that matters, and it is cut.

**Not fixed here** (see Alternatives). It needs: a real
`POST /api/invoice-drafts/:id/promote` that mints the invoice and deletes the
draft in one transaction, recording the `drf_*` (and through it the `dsp_*`) on
the invoice's own audit entry — additively, no new `EventKind` per ADR-0094.

**Owner decision required.** This is a billing-pipeline change; it is Ervin's
call whether it is scheduled, and at what priority relative to the rest of the
Defense backlog. A backlog entry is added by this ADR's doc slice.

### F2 — the no-store decision has an unstated retention horizon

Surfaced by D2's honest branch. Because ADR-0199 §D7 stores no bytes, a report's
document exists only for as long as the renderer that produced it does.
**Bumping `aberp-qc-pdf` makes every previously issued report permanently
unreproducible by ABERP** — the SHA stays pinned in the chain and remains
verifiable *against a copy someone kept*, but the application can no longer
produce one.

That is a defensible trade. It is not what §D7 says: it says the bytes are
derivable, and they are derivable *from a renderer version that may no longer
exist*. §D7 should record the horizon, and someone should decide between
(a) accepting it, (b) keeping superseded renderer versions compilable behind a
feature so an old layout can still be reproduced, or (c) storing bytes for
issued reports after all.

**Not this ADR's call**, and not blocking: the export names the omission per
document (D2/D3), so the horizon is visible in the artifact rather than silent.
Recorded here and in the backlog for Ervin.

## Build slices

1. **Slice 1 — `Ledger::into_connection` + manifest v2 read/write.** The
   consuming accessor with its own unit test. `MANIFEST_VERSION` → 2 with
   `scope_kind` / `scope_id`; the invoice exporter emits the new shape;
   `aberp-verify` accepts the known-versions **set** `{1, 2}` and normalises a v1
   manifest to `scope_kind: "invoice"`. Pinned by a v1 golden and a v2 golden
   verifying side by side, plus a malformed-v1 (`invoice_id` absent) failing.
   No new subcommand yet — this slice is byte-visible only in the manifest, and
   its whole risk is B2, so it lands and gates alone.
2. **Slice 2 — the join, as a pure function.** `dispatch_slice(entries, dsp_id)`:
   pass 1 over the named field set (`dsp_id` / `shipment_id` / `entity_id`),
   pass 2's one hop over `qcr_id`, the empty-id filter on insert, the `seq`
   re-sort, and D1b's shipped-or-refuse. No I/O. Pinned by unit tests including:
   an `export.*` row of each of the three shapes landing in the slice (the B1
   revert-proof), an empty-`qcr_id` payload not sweeping the ledger, a
   `superseded_by_qcr_id` not expanding the set, and a created-only dispatch
   refusing.
3. **Slice 3 — the subcommand.** `export-shipment-bundle`, wiring slices 1–2 to
   `render_report` and `pack_bundle`, D2's 2×2 verdict, D3's omission block, D6's
   edition gate. End-to-end: ship a dispatch with a bound report, export, and run
   the real `aberp-verify` over the produced archive. The B3 revert-proof is a
   test that a stored-`renderer_version` mismatch **omits and names** while an
   equal-version SHA mismatch **refuses**.
4. **Slice 4 — docs.** Flip D-99 residual 1 in
   `docs/BACKLOG-designed-to-live.md`; add the F1 and F2 entries; update
   `README.md` if a row moves.

Slices 1–3 each run the full local gate suite before integrating.
