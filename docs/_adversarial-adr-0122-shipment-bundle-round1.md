# Adversarial review — ADR-0122 round 1 (2026-09-10)

**Verdict: FIX-FIRST.** The scope decision is sound and survives the pass — the
invoice→dispatch severance (F1) is real, verified in four places, and moving the
bundle's scope to the dispatch is the honest response rather than a workaround.
Three mechanisms inside the decision are wrong, and each fails **silently or
totally** rather than loudly and partially, which is the wrong direction for an
evidence artifact. Fix all three in the ADR before any code.

Everything below was checked against the tree at `d638c91`, not reasoned from
the ADR's own text.

---

## B1 (blocking) — pass 1's `dsp_id` probe silently drops the entire `export.*` family

**Claim under test.** D1: "Pass 1 … Every entry whose payload carries
`dsp_id == <target>`. It sweeps `mes.dispatch_created`, `mes.dispatch_shipped`,
the `export.*` family fired at the same boundary, and
`qcr.report_attached_to_shipment`."

**Measured.** The `export.*` family does not use `dsp_id`. Three payloads, three
different shapes (`crates/aberp-dispatch/src/audit.rs`):

| payload | key field(s) |
|---|---|
| `ExportShipmentLoggedPayload` (`:215`) | `shipment_id` — doc comment: "The shipment key — the `dsp_id`" |
| `ExportClassificationSetPayload` (`:117`) | `entity_kind` + `entity_id` |
| `ExportAccessCheckPayload` (`:165`) | `entity_kind` + `entity_id` |

`entity_kind` is `"dispatch"` at the firing site (`apps/aberp/src/serve.rs:19649`).

**Consequence.** A `dsp_id`-only probe matches **none** of the three. The
shipment bundle would ship with its QC documents and without the denied-party
screening decision, the ECCN/authorisation determination, and the export
shipment record — the rows a defense auditor is most likely to have come for.
Nothing in the archive would say they were omitted; the manifest's
`entries_in_bundle` would be a smaller number nobody has a reference for.

This is the same failure the invoice bundle's `BundleMembershipProbe` documents
as load-bearing ("a future payload type that introduces a new id-shaped field
MUST extend this struct in the same PR",
`apps/aberp/src/export_invoice_bundle.rs:147`) — and it is worth noting that
that struct's own guard **has already failed once**: its hand-listed pinning
test `probe_field_set_covers_every_payload_id_field` (`:1378`) does not know
about `DispatchShippedPayload::spawned_invoice_id`, which is an invoice-id-shaped
field it never listed. A hand-listed test does not catch a field nobody thought
to hand-list.

**Required fix.** Pass 1 takes a named, hand-listed field set — `dsp_id`,
`shipment_id`, `entity_id` — mirroring `BundleMembershipProbe`'s discipline and
carrying the same load-bearing warning in its doc comment. Ids are
prefix-namespaced (`dsp_<ULID>`), so plain equality on `entity_id` is sound
without reading `entity_kind`; read it anyway as belt-and-braces, and say in the
comment that the prefix is what makes equality safe so a later contributor who
removes the discriminant knows what they are leaning on.

The ADR must also state the field set **in the ADR**, not only in code: it is a
decision, and a reviewer of the next payload type needs to find it here.

---

## B2 (blocking) — D5's version bump breaks every archive that already exists

**Claim under test.** D5: "`MANIFEST_VERSION` goes to **2** … An older
`aberp-verify` reading a newer bundle fails loudly — the same deliberate posture
ADR-0199 already accepted."

**Measured.** The verifier does not range-check; it equality-checks.

```
crates/aberp-verify/src/bundle.rs:49   pub const SUPPORTED_MANIFEST_VERSION: u32 = 1;
crates/aberp-verify/src/verify.rs:114  if manifest.version == SUPPORTED_MANIFEST_VERSION { ok } else { fail }
```

**Consequence.** The ADR reasoned about one direction (old verifier, new bundle)
and missed the other. Both are broken:

- Ship the writer bump alone → **every** bundle, invoice and shipment alike,
  fails the version check on the current verifier.
- Bump `SUPPORTED_MANIFEST_VERSION` to 2 as well → **every v1 archive already
  written** now fails on the only verifier that exists. Those archives are the
  artifact whose whole purpose is to be verifiable years later. Breaking them to
  add a field is not a trade this ADR is entitled to make.

The forward-compatibility note the constant's doc comment promises ("a newer
`aberp-verify` may understand this bundle") describes a verifier that accepts a
**set**. It does not.

**Required fix.** The verifier accepts a set of known versions — `{1, 2}` — and
normalises on read: a v1 manifest has no `scope_kind`/`scope_id`, so it is read
as `scope_kind: "invoice"`, `scope_id: invoice_id`. New fields carry
`#[serde(default)]` so a v1 document deserialises. `invoice_id` becomes
`Option<String>` on the struct but is still **required in practice for v1** —
a v1 manifest without it is malformed and must fail, not default to `None`.

Pin it: a v1 golden manifest and a v2 golden manifest, both verifying, in the
same test.

---

## B3 (blocking) — D2's refusal fires on a legitimate renderer upgrade

**Claim under test.** D2: "On `Some(false)` the export refuses."

**Measured.** `render_report` (`apps/aberp/src/qc_report.rs:541`) computes
`matches` by comparing the re-rendered SHA against the stored
`rendered_sha256`, and nothing else. A change to `aberp-qc-pdf`'s layout changes
every byte of every report ever issued.

**Consequence.** The first shipment-bundle export after any renderer change
refuses — for every dispatch, forever, with no operator remedy. A refusal that
cannot be cleared is not a safety property; it is an outage in the tool an
auditor is waiting on. And it fires precisely when the operator is least able to
diagnose it, because the message would say "tampered" about a routine deploy.

**What makes the fix available.** The codebase has already anticipated exactly
this question and built the discriminator — which the ADR failed to use.
`crates/aberp-qc-pdf/Cargo.toml:4-10` deliberately pins the crate's own version
instead of inheriting the workspace's `0.0.0`, with this reasoning verbatim:

> every issued report recorded `renderer_version = "0.0.0"` — which made the
> field useless for the one question it exists to answer: when a re-render's SHA
> does not match the pin, was the RENDERER changed or were the rows TAMPERED
> with? … **Bump it in the same commit as any change to the rendered bytes.**

`renderer_version` is stored on the row and on the `qcr.report_issued` payload
(`crates/aberp-qa/src/qc/reports.rs:1152`), and it is printed into the page
footer, so a bump necessarily changes the bytes — the signal is self-consistent.

**Required fix.** Split the verdict on the stored `renderer_version` versus
`aberp_qc_pdf::QC_PDF_RENDERER_VERSION`:

- **equal, SHA differs → REFUSE.** Same renderer, different bytes: the frozen
  rows moved under a report that is supposed to be frozen. This is the tamper
  signal and it deserves the loud stop.
- **different, SHA differs → OMIT and NAME it**, through D3's
  `qc_documents_omitted` block with `reason: "renderer_version <stored> is no
  longer available (current <current>) — the issued bytes cannot be
  reproduced"`. The bundle is still produced; the auditor is told exactly which
  document is missing and why, and the chain entry with the original SHA is
  still in `chain.jsonl`.
- **equal, SHA equal → bundle it.** The ordinary path.
- **different, SHA equal** cannot happen (the version prints into the page), but
  handle it as the ordinary path rather than asserting — an assertion here buys
  nothing and can only turn a harmless surprise into an abort.

---

## C1 (non-blocking, fix in the same pass) — a never-shipped dispatch produces an "evidence" bundle

Pass 1 matches `mes.dispatch_created`, so a Drafted or cancelled dispatch yields
a one-entry slice. That is not empty, so ADR-0029 §2's zero-entry loud-fail does
not catch it, and the operator receives an archive named for a shipment that
never happened, containing no reports and no export decisions — indistinguishable
in shape from a shipment whose documents were dropped.

**Fix.** Refuse unless the slice contains a `mes.dispatch_shipped` for the
target, with a message naming what the dispatch's ledger actually shows. Note in
the ADR that this deliberately leaves a cancelled dispatch with no export path;
that is a real gap and belongs in Open questions rather than in a silent
success.

## C2 (non-blocking) — pass 2 needs the empty-string guard its sibling has

`BundleMembershipProbe::matches` refuses an empty target explicitly, as
"defence-in-depth — a tampered payload with an empty `invoice_id` field would
otherwise match every empty-target query" (`export_invoice_bundle.rs:180`). Pass
2 builds its id set *from payloads*, so the same hazard arrives from the other
side: one payload with `"qcr_id": ""` puts the empty string in the set, and every
entry with an empty `qcr_id` joins the slice. Filter empties out of the set on
insert, not only on compare.

## C3 (record, do not fix) — F2: the no-store decision has an unstated retention horizon

B3's honest branch has a consequence worth naming beyond this ADR. Because
ADR-0199 §D7 stores no bytes, a report's document exists only while the renderer
that produced it does. **Bumping `aberp-qc-pdf` makes every previously issued
report permanently unreproducible** — the SHA stays pinned in the chain and
verifiable *against a copy someone kept*, but ABERP itself can no longer produce
one.

That is a defensible trade and it is not what §D7 says. It says the bytes are
derivable. They are derivable *from a renderer version that may not exist any
more*. ADR-0199 should record the horizon, and someone should decide whether the
answer is (a) accept it, (b) keep old renderer versions compilable behind a
feature, or (c) store bytes for issued reports after all. Not this ADR's call.

---

## Confirmed sound — pressed and held

- **D4's ordering.** `detect_mirror_agreement(db_path: &Path, db_entries: &[Entry])`
  (`export_invoice_bundle.rs:354`) takes a path and already-read entries, not the
  `Ledger`. Consuming the `Ledger` after `entries()` strands nothing. The
  consuming-vs-borrowing argument holds: a `&Connection` accessor would hand out
  a writer with its provenance stripped, which is the shape CHECK 10P exists to
  red.
- **Pass 2 terminates and does not silently grow.** Exactly five payload sites
  carry `qcr_id` (`reports.rs:1015`, `:1130`, `:1205`, `:1239`, `:1281`) —
  frozen, issued, attached, rendered, voided. The one field that could expand the
  set, `superseded_by_qcr_id` on the void payload (`:1284`), has a **different
  name**, so a one-hop rule keyed on `qcr_id` cannot chase it. The "one declared
  hop, no closure" framing is not just a policy here; it is what the field names
  already enforce.
- **F1 itself.** Verified independently at four sites — the spawner returning a
  `drf_` id (`invoice_draft.rs:658`), the form-fill promotion route comment
  (`serve.rs:5188`), `issue_invoice` carrying no draft reference, and the delete
  NULLing the dispatch pointer (`invoice_draft.rs:555`). The exporter's own
  exhaustiveness arm (`export_invoice_bundle.rs:722`) records the first half of
  it. Not a misreading.
- **D6's edition gate.** `render_report` already calls
  `assert_qc_reporting_allowed` first, so a Portable build cannot reach a report
  row even if the subcommand were reachable. Gating the subcommand is
  belt-and-braces, not the only guard.

## Verdict

**FIX-FIRST.** Apply B1, B2, B3, C1 and C2 to the ADR, record C3 as F2, and
re-read. The scope decision, F1, D4 and the pass-2 bound need no change.
