# Adversarial review — VAT rate-kind port (Editions `main` @ `2ca3030`, PR #27)

Reviewed at `2ca3030` in an isolated worktree. Gates run with all `ENFORCE_*` on:
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -D warnings`,
full `cargo test --workspace`, SPA `vitest run` (81 files / 1458 tests),
`tools/cut_gate_db_isolation.sh` — all green, opener census unmoved
(114 frozen openers / 101 fingerprints / 0 write-fork / 14 mirror-fork sites).
`cut_gate_negative_probes.sh` deliberately NOT run (shared `$TMPDIR`).

## What holds

- **The widened bucket key is a genuine strict superset.** MUT1 (narrow the key
  back to `basis_points`) → all 6 mixed-kind pins RED, all 8 B3′ pins GREEN.
  Both directions demonstrated. The inherited B3′ pin file
  (`nav_xml_summary_multibucket.rs`) differs from its pre-port form by exactly
  the two new struct fields, on the `Percent` path — verified by diffing
  `9ae548d..2ca3030` on that file. Neither too wide (an all-`Percent` invoice
  over `{0, 5, 18, 27}` yields exactly one bucket per distinct rate, and two
  lines sharing `(kind, rate)` still merge — PR #26's pin) nor too narrow (six
  distinct `(kind, rate)` groups on one body → six buckets).
- **The four 0%-kinds each land in their own bucket with the right NAV
  category**, on a genuinely mixed body: AAM → `vatExemption/AAM`,
  KBAET → `vatExemption/KBAET`, EUFAD37 → `vatOutOfScope/EUFAD37`,
  §142 → `vatDomesticReverseCharge`.
- **B2 (Invariant V)** — `vat_amount()` gates on kind at the derivation;
  `gross_total()` composes through it; the only two consumers are the per-line
  emit (`nav_xml.rs:1658`) and the summary accumulator (`nav_xml.rs:1970`).
  No wire figure is derived from `basis_points` behind its back.
- **B4 (Invariant I)** — normalisation happens at all three library ingest
  points (`issue_from_parsed`, `storno_from_inputs`, `modification_from_inputs`),
  the `Err` arm is untouched, malformed numbers still reject, and a VIES-style
  spaced/lowercase number stays accepted. See finding 3 for the one artifact
  that does not get it.
- **Storno and modification bucket mixed-kind correctly** through the shared
  `write_summary`: three buckets each, storno negated per bucket, modification
  full-replace unnegated, both pass the local validator.
- **Mutation integrity** — MUT1, MUT2, MUT3, MUT4, MUT6 reproduce exactly as the
  port's commit message records, including the two corrected pins. See finding 5
  for the one that does not.

## Findings, ranked by whether they file a wrong invoice to NAV

### 1. Modification route files an unvalidated VAT rate-kind — FIXED in this PR

`serve::modification_invoice_request`'s ADR-0101 / S2 guard reads the **base**
invoice's persisted kinds. A `Percent` base passes it, and step 5 then builds the
modification body from `request.lines` — whose `vat_rate_kind` is a
`#[serde(default)]` field on `LineJson`. This route calls
`validate_invoice_preflight` nowhere (Invariant P, deferred — ADR-0103 §6.1), so
neither `MixedVatRateKindsUnsupported` nor the ADR-0102 §4(a) buyer-status matrix
sees the body.

Reproduced against the real route: a two-line modification of a `Percent` base
(`Percent 27%` + `IntraCommunityGoods`) against a **DOMESTIC** buyer with an HU
tax number was ACCEPTED and emitted

```xml
<customerVatStatus>DOMESTIC</customerVatStatus>
...
<summaryByVatRate>
  <vatRate><vatExemption><case>KBAET</case>
    <reason>Közösségen belüli adómentes termékértékesítés [Áfa tv. 89. §]</reason>
  </vatExemption></vatRate>
  <vatRateNetData><vatRateNetAmount>50000</vatRateNetAmount>…
```

i.e. 50 000 Ft of domestic taxable turnover filed to NAV as an intra-Community
exempt supply — the exact combination §4(a) calls unsatisfiable. New surface as
of this port (`vat_rate_kind` did not exist on `LineJson` before `2ca3030`).

Fixed here by a request-side guard mirroring the base-side one. Byte-identical
for real SPA traffic: `composeModificationBody` never emits `vatRateKind`, so a
form-originated body always deserialises every line as `Percent`. Mutation-verified.

### 2. The printed PDF drops the VAT rate-kind entirely — OPEN

Gated-reachable, not a bypass: `validate_invoice_preflight` accepts a **uniform**
non-`Percent` invoice (pinned by
`mixed_kinds_rejected_but_uniform_non_percent_accepted`), and the SPA composes
`vatRateKind` from its VAT-type selector. An all-AAM invoice is an ordinary
issuable invoice today.

`print_invoice::parse_nav_invoice_xml` handles `<vatPercentage>` only. For a line
carrying `<vatExemption>` / `<vatOutOfScope>` / `<vatDomesticReverseCharge>` it
leaves `vat_rate_percent` at its `Default` — `0` — silently, with no loud-fail.
Verified for all four wired kinds. Consequences on the paper document:

- the line's ÁFA column prints `0%` (`invoice-pdf/src/lib.rs:670`) with no
  exemption / reverse-charge reference — Áfa tv. §169 requires one;
- the ÁFA summary block buckets on `vat_rate_percent`
  (`invoice-pdf/src/lib.rs:729`), so all four 0%-kinds collapse into a single
  `0% ÁFA:` row while NAV receives four categorised buckets.

The NAV filing is correct; the printed document contradicts it categorically.
Not fixed here — carrying the kind onto the PDF is a design change to the
regulatory document surface (Hungarian exemption wording, layout), not a
surgical patch. Recommended shape: capture the choice element + `<case>` in the
PDF-side parser exactly as `nav_xml::read_invoice_lines_from_xml` already does
for the storno fold, and render the ADR-0101 `reason` text in place of `0%`.

### 3. B4's Invariant I misses the third artifact on the SPA issue route — OPEN

`normalize_customer_community_vat_number`'s doc-comment claims the bytes
"validated, persisted to the side-store, stamped on the audit payload and
written to the NAV XML" are the same bytes, and the port's commit message
rejects emit-time normalisation because "it would leave the side-store and audit
payload raw". But `handle_issue_invoice` writes the side-store `input.json` in
`serve.rs` **before** calling `issue_from_parsed`, which is where the
normalisation runs. Observed on the real route:

```
side-store input.json  →  "communityVatNumber":"at u123 45678"   (raw)
NAV XML                →  <communityVatNumber>ATU12345678</…>    (normalised)
audit payload          →  "ATU12345678"                          (normalised)
```

Two of three artifacts. Impact is contained — chain replays re-normalise at
their own ingest, so storno/modification children are unaffected and nothing
wrong reaches NAV. Fix is one line: call
`issue_invoice::normalize_customer_community_vat_number(&mut input)` in
`serve.rs` immediately before the side-store write (keeping the existing
write-before-issue ordering). Left out of this PR to keep it to one defect.

### 4. Printed vs filed HUF gross — real, arithmetic-guaranteed, NOT port-introduced

Definite answer to the open `editions-eur-gross-huf-printed-vs-filed` question.

- Printed: `RateMetadata::huf_equivalent_total` = **one** round-half-even
  conversion of the native grand gross (`issue_invoice::finalize_rate`), rendered
  as `Bruttó összeg: X Ft` (`invoice-pdf/src/lib.rs:812`).
- Filed: `<invoiceGrossAmountHUF>` = **sum of per-bucket** conversions
  (`nav_xml::write_summary`).

These diverge whenever the per-bucket rounding residuals do not cancel. Measured
at MNB rate `356.690000` over unit prices 1–200 c:

| shape | divergent pairs | gated? |
|---|---|---|
| mixed-**RATE** EUR, all `Percent` (5% + 27%) | 9905 / 40000 (~25%) | **yes — ordinary SPA invoice** |
| mixed-**KIND** EUR (AAM + KBAET) | 10076 / 40000 (~25%) | no — needs an ungated door |

So the port does **not** meaningfully widen reachability: the same ~25% chance
already existed on any two-bucket EUR invoice from PR #25 (B3′) onward, via a
completely ordinary mixed-rate body, while the mixed-kind arm sits behind the
`MixedVatRateKindsUnsupported` gate. The repo's own mixed-kind pin already
encodes an instance of it (`huf_equivalent_total: 2854` vs a filed gross of 2853).

**It does not file a wrong or internally-inconsistent invoice to NAV.** The NAV
body is self-consistent — every figure on it is the sum over buckets, computed
one way. The mismatch is paper-vs-filing on an informational line. The
legally-required figures do not diverge: the PDF converts VAT **per rate bucket**
with the same `huf_equivalent_round_half_even`
(`invoice-pdf/src/lib.rs:765`), which is the same partition NAV gets for an
all-`Percent` invoice, and every non-`Percent` bucket carries zero VAT. No
invoice-level HUF VAT total is printed.

For completeness: `net_HUF + vat_HUF != gross_HUF` on 39% of gated mixed-rate EUR
pairs — but also on ~25% of **single-bucket** invoices, so that is a pre-existing
property of independently rounding three derived amounts (PR-44γ), untouched here.

### 5. Two mixed-kind pins assert against the line block, not the summary — OPEN

`two_exemption_kinds_keep_distinct_case_codes` and
`all_four_zero_percent_kinds_each_emit_their_own_category` assert on bare
substrings like `"<vatExemption><case>KBAET</case>"` against the compacted
**whole body**, which the per-line `<lineVatRate>` block also satisfies. Under a
mutation that keeps the bucket count but writes `lines.first()`'s choice into
every bucket's `<vatRate>` — precisely the B3′ category defect — both pins stay
GREEN. Only `bucket_count` distinguishes, and that mutation does not move it.

The port's MUT5 (a full collapse to one bucket) reddens them via `bucket_count`,
which is why the record reads "all 6 mixed-kind pins RED"; the narrower
category-only mutation reddens 3 of 6. The escape is closed by the sibling
`two_zero_percent_kinds_emit_two_buckets_in_different_categories`, which anchors
its assertions to `<summaryByVatRate><vatRate>` — so the file as a whole is not
blind and nothing on `main` is wrong. Fix: anchor the two pins the same way.
