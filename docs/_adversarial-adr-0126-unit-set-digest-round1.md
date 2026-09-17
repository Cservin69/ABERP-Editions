# Adversarial review — ADR-0126 round 1 (2026-09-16)

**Verdict: FIX-FIRST.** The decision survives — a digest over the enumerated
units, length-prefixed, stored in two places, is right. But **D3 rests on an
enforcement that does not exist**, and the review found a third copy of an
overclaim ADR-0124 was supposed to have retired. Two more amendments are
needed before any code.

Everything below was checked against the tree at `86d95c1`.

---

## A1 (BLOCKING — the ADR asserts a gate that does not cover this payload)

**Claim under test.** ADR §D3: adding the payload field without updating the
`EventKind` doc comment reds the build, "enforced by
`export_payload_field_names_match_the_eventkind_docs`".

**Measured: false.** That test
(`crates/aberp-dispatch/src/audit.rs:328`) pins **only the three `export.*`
payloads**, each a typed struct with a hardcoded expected key list. It does not
scan doc comments, and it never touches `qcr.report_issued` — which is built
inline with `json!{…}` in `reports.rs:1128`, not as a typed payload struct.

`grep` confirms **nothing anywhere pins that payload's key set.**

So on the `qcr.*` family the doc comment is unenforced prose. The codebase's
"the doc comment IS the schema contract" claim is true where it is *tested* and
aspirational where it is not — and this is one of the latter.

**Required, and it is an improvement rather than a retreat:** create the
enforcement instead of citing it. Add a pin that freezes + issues a report and
asserts the emitted `qcr.report_issued` payload's exact key set, on the same
"no more, no fewer" discipline the export test uses. That both protects this
change and closes a gap that was open before it.

Rewrite D3's justification to say the doc is updated **by discipline and by a
new pin added here**, not by an existing gate.

---

## A2 (BLOCKING, NEW — ADR-0124's correction missed a third copy, in the contract itself)

`QcReportIssued`'s doc comment
(`crates/audit-ledger/src/entry/event_kind.rs`) still reads:

> "The BYTES ARE NOT STORED anywhere — the report re-renders deterministically
> from the frozen `qc_report_lines`, and this chain entry proves the bytes
> anyone re-renders are the bytes that were issued (ADR-0199 §D7)."

**That is the exact sentence ADR-0124 exists to correct.** ADR-0124 §3 states
the claim is corrected "in both places" — ADR-0199 §D7 and
`apps/aberp/src/qc_report.rs:397`. There is a **third** copy, and it is the
worst of the three: it sits in the artifact this codebase calls its schema
contract, on the load-bearing event.

It is unconditionally stated and conditionally true, for precisely the reason
ADR-0124 gives: the bytes are reproducible only by the renderer version
recorded on the entry, and after a renderer bump the prior bytes are not
reproducible at all.

**Required:** correct it here, marked and dated, on the ADR-0123 §D8 precedent.
This ADR is already editing that doc comment to add the payload field, so
leaving the stale claim beside the new field would be a deliberate omission.

---

## A3 (FIX — the ADR says "set"; the data is a MULTISET)

**Claim under test.** §D2 sorts "pairs" and calls the result a set.

**Measured:** `wo_part_marks` has **no primary key and no unique constraint of
any kind** — not on `(tenant_id, wo_id, unit_index)`, not on `part_uid`, not on
`serial_number`. Nothing at the schema level stops two rows sharing a
`part_uid`, or a row being duplicated outright. `record_part_marks`'s
refuse-second-write is the only thing holding that line, and it is exactly the
protection this ADR assumes is absent.

The proposed encoding already handles this correctly — it sorts and encodes
**every** pair, so a duplicate changes the digest — but the ADR must say
*multiset*, not *set*, or a future reader will "optimise" it with a
dedupe and silently reopen the hole.

**Required:** state multiset semantics explicitly, and pin a duplicated-row
case in the tests.

---

## A4 (FIX — D5's "records that the strong check did not run" is decoration as written)

§D5 says a report without a digest falls back to the range comparison "and
records that the strong check did not run". The ADR never says **where**. A
degradation that exists only in a code comment is not visible to an auditor,
and §4 surface 6 asked this of itself without answering it.

**Required:** name the surface. The cheapest honest one that matches existing
practice is ADR-0122 §D3's: the evidence bundle already has a machine-readable
omission block that names what is missing and why. A legacy report should
appear there as verified-by-range-only. If that is out of scope for this slice,
then D5 must say the degradation is **not** surfaced and that legacy reports
are indistinguishable at the API — an honest absence, not an implied feature.

---

## A5 (clears) — the identity claim, verified at both sites

The ADR asserts `part_uid` is what the measurement join and the NCR belt key
on. Both confirmed rather than taken from the residual's summary:

- `crates/aberp-qa/src/qc/reports.rs:350` —
  `Evidence::Unit(uid) => linked_part_uid == Some(uid)`;
- `apps/aberp/src/serve.rs:18190` —
  `open_ncr_ids_blocking_wo(&ncrs, &waivers, &dispatch.wo_id, &part_uids)`.

So shape B is not merely a key collision: after it, the measurements that
justified `accept` no longer join to the units being shipped, and the NCR belt
is looking at uids that are no longer marked.

---

## A6 (clears) — D2b is well founded

Both sides derive units identically from `list_part_marks`
(`qc_report.rs:213` at freeze, `serve.rs:18543` at check), and a work order
with no marks yields an empty `Vec`, not a `None`. So "the empty set has a
digest" is implementable exactly as written, and the
lot-only-versus-legacy ambiguity D2b exists to prevent is real and is closed
by it.

---

## A7 (clears, with a required pin) — injectivity

`str::len()` is bytes on both sides, so the length prefix and the content agree
for any UTF-8 including multi-byte serials, and the empty string encodes as
`0:` unambiguously. The encoding is injective.

But this is the whole fix, so it does not ship on an argument.
**Required:** a test that constructs the collision a delimiter join would
produce — `("AB","C")` against `("A","BC")` — and asserts the two digests
differ.

---

## A8 (deferred to the build, with a named test) — rendered bytes

§D4 asserts the rendered bytes do not move. The renderer does read
`serial_range` twice (`aberp-qc-pdf/src/lib.rs:344`, `:548`), and the new field
is not in the template — but "not in the template" is an argument, and ADR-0124
is the standing reminder of what a moved byte costs.

**Required:** a test that renders a report before and after the field exists
and asserts the SHA-256 is unchanged. Not optional; it is a one-line assertion
against the thing the whole `rendered_sha256` pin depends on.

---

## Confirmed sound — pressed and held

- **Digest over rendering.** The bug is entirely the consequence of using a
  human projection as an equality test; D1 is the right correction.
- **Both storage locations.** The column answers "did the marks move", the
  ledger entry answers "did the column move". Neither subsumes the other.
- **No backfill.** Reconstructing a frozen set from marks-as-they-are-now
  reads the very thing under suspicion. ADR-0123's posture applies exactly.
- **`serial_range` untouched.** It is inside hash-pinned bytes; changing it to
  carry more would move every report's rendering.
- **Scope.** One comparison site, one freeze site, one column, one payload
  field, Defense only.

## Verdict

**FIX-FIRST.** Rewrite D3 to create the enforcement rather than cite a gate
that does not cover this payload (A1); correct the third copy of ADR-0124's
overclaim in the `EventKind` contract (A2); say multiset (A3); make D5's
degradation either real or honestly absent (A4). Add the three required pins:
the injectivity collision (A7), the rendered-bytes invariance (A8), and the
payload key set (A1). D1, D2, D2b, D4, D6 and D7 stand.
