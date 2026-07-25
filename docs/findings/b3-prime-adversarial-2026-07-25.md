# FINDING — adversarial review of B3′ (`summaryByVatRate` per-rate bucketing)

- **Date:** 2026-07-25
- **Subject:** `b4f37a3` + `5c266e3`, merged to Editions `main` as `f7c216e`
  (PR #25). The adversarial review was skipped when the merge session
  auto-archived; this is that review, run against `f7c216e`.
- **Verdict:** the fix **holds**. The B3′ arithmetic, the `nav-xsd-validator`
  loosening and the recorded landmine are all correct. One mutation-integrity
  hole was found and closed (§1). Two residuals are recorded here, neither of
  which files wrong ÁFA (§2, §3).
- **Gates:** full Portable + Defense (`--features production`) arms —
  fmt, `build --workspace --locked --all-targets`, `test --workspace`, the
  named integration tests, `clippy -D warnings`; plus the ADR-0093 cut-gate
  with all five `ENFORCE_*` on, its negative probes, and the SVG lints.

## 1. CLOSED — same-rate bucket MERGING was unpinned

Invariant S has two halves: "one bucket per distinct rate" and "**only** one".
Every multi-rate pin in `nav_xml_summary_multibucket.rs` carried exactly ONE
line per rate, so an emitter that never merges — pushing a fresh bucket per
line — satisfied the entire suite. Verified by mutation: replacing the
`buckets.iter().position(..)` lookup with an unconditional push left `aberp` +
`aberp-nav-xsd-validator` + `aberp-billing` **fully green, zero failures**.

The behaviour itself was never wrong; the coverage was. It matters because a
multi-line invoice at one rate is the commonest invoice ABERP issues, and
because that lookup is exactly what the parked
`port/vat-rate-kind-s1-machinery` merge has to hand-resolve (§2).

Closed by three pins in the same file — same-rate merge, the 27/18/0 three-rate
shape, and the modification render path — each mutation-verified.

## 2. RESIDUAL — the parked rate-kind port, and its stale plan line

`git merge-tree origin/main port/vat-rate-kind-s1-machinery` **conflicts** in
`apps/aberp/src/nav_xml.rs`. That is the desired outcome: the parked branch
still carries the pre-fix `if let Some(first) = lines.first()` collapse, and it
cannot land silently. `docs/PORT-PLAN-vat-rate-kind-eu-partner.md` **on that
branch** still records `apps/aberp/src/nav_xml.rs | +550 | CLEAN`, which is now
false, and carries no note that the bucket key must be widened.

Whoever lands the port must, in the conflict resolution:

1. widen the bucket key from `basis_points` to `(vat_rate_kind, basis_points)`
   and sort on the pair — `VatRateKind` is a payload-free
   `Copy + Eq + Hash` enum, so this is a mechanical widening, not a rewrite;
2. keep the merge (do not reintroduce a per-line push) — §1's pins guard this;
3. add `vat_rate_kind` to the `LineItem` literals in
   `apps/aberp/tests/nav_xml_summary_multibucket.rs`, which will not compile
   otherwise. **Do not delete that file to clear the error.**

## 3. RESIDUAL — printed vs filed HUF gross on a mixed-rate foreign-currency invoice

B3′ made the invoice-level `*HUF` figures the SUM of the per-bucket HUF, per
ADR-0037 §1.c ("Invoice-level total HUF amount … the sum of the per-VAT-rate
HUF amounts, NOT by converting the EUR invoice total directly"). The wire body
is internally consistent under that rule: `invoiceGrossAmountHUF` equals
`invoiceNetAmountHUF + invoiceVatAmountHUF`.

`RateMetadata::huf_equivalent_total` — stamped by
`issue_invoice.rs::finalize_rate` and by
`invoice_currency_metadata.rs::inherit_rate_metadata_for_chain` — is still a
SINGLE `huf_equivalent_round_half_even` of the grand gross. It is what the
printed invoice shows as *"Bruttó összeg"* (`crates/invoice-pdf/src/lib.rs`,
Áfa tv. §80(1)(g)) and what the SPA renders.

On a single-bucket invoice the two are identical, which is every invoice issued
to date. On a mixed-rate non-HUF invoice they can differ by 1 Ft per bucket
boundary. Measured example, MNB 356.690000, 1.00 EUR @ 5% + 7.00 EUR @ 27%:

| figure | value |
| --- | --- |
| wire `invoiceGrossAmountHUF` (Σ per-bucket) | 3546 |
| printed / stored `huf_equivalent_total` | 3545 |

About a quarter of round-euro two-line mixed-rate invoices diverge this way.
Not an ÁFA figure — `invoiceVatAmountHUF` and the PDF's per-rate ÁFA-in-HUF
rows both convert per rate and agree — and not reachable before B3′, because a
mixed-rate invoice did not produce a correct multi-bucket body at all.

**Not fixed here.** The wire is the ADR-0037-§1.c-correct side; bringing
`huf_equivalent_total` in line means changing the issuance path, the chain
inheritance and the printed invoice, which is well outside B3′'s surface.
Closing step: an ADR-0037 amendment naming which of the two is canonical for
§80(1)(g), then a single change to `finalize_rate` +
`inherit_rate_metadata_for_chain` if the sum-of-buckets side wins.

## 4. RESIDUAL — ADR-0103 / ADR-0101 are cited in source but do not exist here

`ADR-0103` (7 citations) and `ADR-0101` (2) are the **only** ADR references in
Editions source that do not resolve against `adr/`; every other citation in the
tree does. Both are ABERP.git-line ADRs. The load-bearing compliance rule for
the ÁFA summary ("ADR-0103 §3.1 — Invariant S") therefore points at a document
this repo does not carry, against the ADR-0093/SAW-OFF self-containment
posture. Closing step: port ADR-0103 (at least §3.1) into `adr/`, or add a
stub that names the prod-line source.

## 4b. CLOSED (comment only) — "once each" was an overclaim

B3′ added, above `check_ordered_required(PARENT, ORDERED_INVOICE_AMOUNTS, ..)`:
*"The four invoice-level amounts: present, once each, in order."* The helper
projects `seen` onto `required` and compares the first `required.len()`
positions, so a duplicate TRAILING amount is accepted. Probed directly: a
`MIN_VALID` body with `invoiceVatAmountHUF` twice **validates**.

Behaviourally this is the shared helper's posture everywhere in the validator
(ADR-0022's deliberate loose model) and the emitter cannot produce a duplicate
— so the code is left alone and the comment now says what is actually enforced.
An overclaiming comment on a compliance validator is the thing worth removing.

## 5. What was checked and held (no change needed)

- **Validator vs. ground truth.** Fetched the published
  `nav-gov-hu/Online-Invoice` `invoiceData.xsd`. `SummaryNormalType` is
  `xs:sequence` of `summaryByVatRate` `maxOccurs="unbounded"` (minOccurs
  defaults to 1) followed by the four invoice amounts, once each, in order —
  exactly what `walk_summary_normal` now enforces. It still rejects a body with
  no bucket, a bucket after an invoice amount, and out-of-order amounts.
  `SummaryByVatRateType` is `vatRate, vatRateNetData, vatRateVatData,
  vatRateGrossData?` — the emitter's order. (Pre-existing, untouched by B3′:
  `walk_summary_by_vat_rate` does not enforce order among a bucket's own
  children, and requires `vatRateGrossData` that the XSD makes optional.)
- **Arithmetic.** 27+5, 27+18+0, several lines per rate, storno, modification —
  one bucket per rate, each summing only its own lines, per-bucket HUF
  conversion, invoice totals reconciling, gross = net + VAT. Single-rate output
  byte-identical.
- **One emitter, both arms.** `write_summary` is the only `summaryByVatRate`
  producer in the tree, reached by exactly three render paths (invoice, storno,
  modification); annulment carries no summary. No `feature = "production"`
  gate touches NAV emission — the `production` cfg sites are confined to
  `build_profile.rs`, `numbering.rs`, tenant guards and boot e2e tests. The
  Defense-only surfaces (firing-site, heat-lot, part-UID) live in the QA /
  traceability modules and do not emit invoice summaries.
- **Rate vocabulary.** `ALLOWED_VAT_RATES_PERCENT = [0, 5, 18, 27]` →
  `{0, 500, 1800, 2700}` bp, so `write_vat_rate`'s `{:.2}` rendering is
  injective over the live vocab and two distinct buckets can never emit the
  same `<vatPercentage>`.
- **Mutation matrix**, whole file, all observed:

  | mutation | red |
  | --- | --- |
  | unconditional bucket push (per-line buckets) | T2, T2b, T3b — and, before §1, *nothing* |
  | pre-fix single-bucket collapse | T1, T2b, T3, T3b, T4, T6 (T2, T5 stay green) |
  | stable sort dropped | T4, T2b |
  | invoice HUF reconverted from the grand total | T6 only |
  | validator restored to the single-bucket positional check | both validator pins |
