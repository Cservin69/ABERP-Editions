# ADR-0103 (Defense) — Completing the VAT rate-kind port: kind-consistent VAT, community-VAT identity, and mixed-*kind* summary bucketing on top of the landed B3′ fix

- **Status:** **Accepted — reconciliation plan. Written and committed BEFORE implementation** (2026-07-25), per the session constraint that a prior VAT session lost its analysis by implementing first. Implementation record in §8, filled in as each step lands.
- **Date:** 2026-07-25
- **Deciders:** Ervin Áben (set the scope: finish VAT correctness on the Defense line; conservative option where ambiguous; no AskUserQuestion). Reconciliation + implementation by Dispatch.
- **Base:** Editions `main` @ `f7c216e` (PR #25 — the B3′ rate-only summary fix + its lock-step validator change). Every file:line below was read in this session at that SHA or at the named branch ref, not inferred.
- **Related:** **ABERP.git ADR-0103** @ `origin/main` (merge `97ecfaf`) — the prod-line original this ports; **ADR-0101 (Defense)** (per-line `vat_rate_kind`) and **ADR-0102 (Defense)** (EU-partner customer type), which arrive on this branch with the port merge (§3); ADR-0037 §1.c (per-bucket HUF); ADR-0049 (chain replay reads the side-stored `input.json`); ADR-0022 (`nav-xsd-validator` is deliberately loose).
- **Governance:** this repo has no `CLAUDE.md`; the Defense obligations live in `SAW-OFF.md`, `FOUNDATION.md` and the `adr/` series. "CLAUDE.md rule N" references inherited from the prod-line text are Portable-repo conventions — read them as the Defense analogues.

---

## 0. TL;DR

The Defense line carries **half** of the prod-line ADR-0103. `f7c216e` landed Invariant **S** in a **rate-only** form — `summaryByVatRate` buckets keyed on `vat_rate_basis_points` alone. The other half is parked and unmerged:

| Invariant | Prod (ABERP.git `origin/main`) | Defense (`f7c216e`) | This ADR |
|---|---|---|---|
| **S** — summary coverage | buckets keyed `(kind, basis_points)` | buckets keyed `basis_points` **only** | **widen the key** |
| **V** — kind-consistent VAT (B2) | landed | **absent** (`vat_amount` is rate-only) | port |
| **I** — one value (B4) | landed | **absent** (the whole community-VAT surface is parked) | port |
| **P** — universal preflight gate (B1a) | **deferred on prod** | absent | **stays deferred** (§6.1) |
| **C** — chain congruence (B1b) | **deferred on prod** | absent | **stays deferred** (§6.1) |

The rate-only S is not wrong — it is **narrower than the invariant**. It partitions correctly for mixed-*rate* invoices and mis-partitions for mixed-*kind*-same-rate invoices, which collapse into one bucket carrying two different NAV categories. Widening the key to `(kind, basis_points)` is a **strict superset** of what `f7c216e` does: for every invoice with a single kind — every invoice the Defense line can currently issue, since the kind machinery is parked — the bucket partition is *identical*, byte for byte.

**This is the load-bearing reconciliation fact, and it is what makes the port safe: the B3′ fix is not reverted, it is extended.**

---

## 1. The landmine — verified, not assumed

The brief warned that the parked branches predate `f7c216e` and still carry the pre-fix single-bucket collapse, so a naive merge would re-introduce B3′. **Verified by trial merge in an isolated worktree at `f7c216e` (aborted; nothing committed).** The result is more precise than the warning, and the precision matters:

| File | Merge outcome | Verdict |
|---|---|---|
| `apps/aberp/src/nav_xml.rs` | **CONFLICT**, exactly one hunk (`:1961-1972`) | **Loud.** Git surfaces precisely the kind-threading line: `write_vat_rate(w, b.basis_points)` (HEAD) vs `write_vat_rate(w, first.vat_rate_kind, first.vat_rate_basis_points)` (parked). Main's whole `for b in &buckets` loop, the `VatRateBucket` accumulation and the per-bucket HUF summing **survive the merge unconflicted.** |
| `crates/nav-xsd-validator/src/validate.rs` | **auto-merged, clean** | **Silent — and it resolved CORRECTLY.** `walk_summary_normal` keeps `f7c216e`'s `ORDERED_INVOICE_AMOUNTS` (buckets excluded from the positional check, `maxOccurs="unbounded"`); `walk_vat_rate` *gains* the parked branch's `vatExemption` / `vatOutOfScope` / `vatDomesticReverseCharge` choice arms. The two changes touch disjoint functions, so both land. |
| `apps/aberp/tests/nav_xml_summary_multibucket.rs` | untouched, survives | The B3′ regression pin is **not** deleted by the merge; it stands as a live tripwire through the whole port. |

**So the landmine is real but not silent in the shape feared.** The one place the parked branch would revert the fix is the one place git refuses to guess. The genuine risk is not an invisible revert — it is a *careless conflict resolution* that takes the parked side wholesale. §4.1 resolves it in the only direction that satisfies the invariant.

> ⚠ **Method note, recorded because it nearly cost this session its own analysis.** The first trial merge was run in the wrong checkout — the main repo on `main` @ `caee622`, which *predates* B3′ — and reported a clean, conflict-free merge with a reverted `write_summary`. That "confirmation of a silent revert" was an artifact of the wrong base, not a finding. It was aborted and the checkout restored. **Every git invocation in this port uses an explicit `git -C <worktree>`**; a bare `cd` is not trusted to persist. A merge that is clean on the wrong base tells you nothing about the right one.

---

## 2. Current state (read at `f7c216e` / the named refs)

| # | Site | What is there |
|---|---|---|
| **S (partial)** | `nav_xml.rs:1870` `struct VatRateBucket` | `{ basis_points: u16, net, vat, gross }` — **no `kind` field.** |
| **S (partial)** | `nav_xml.rs` `write_summary` | groups on `let key = line.vat_rate_basis_points`, `position(\|b\| b.basis_points == key)`, `sort_by_key(\|b\| b.basis_points)`. Correct for mixed-rate; blind to kind. |
| **S (partial)** | `nav_xml.rs:1803` `write_vat_rate` | single-arg `(w, basis_points)` — always emits `<vatPercentage>`. No choice element. |
| **V absent** | `modules/billing/src/domain/invoice.rs` `vat_amount()` | `net × basis_points / 10_000`, unconditional. `vat_rate_kind` **does not exist on `LineItem` at this SHA.** |
| **I absent** | — | `validate_community_vat_number` **does not exist** on `f7c216e`. `git grep community_vat_number` over `apps/ modules/ crates/` returns exactly one hit: a doc-comment mention at `apps/aberp/src/partners.rs:224`. The entire ADR-0102 surface is parked. |
| machinery | `modules/billing/src/domain/vat_rate_kind.rs` | **absent on `f7c216e`**; present on both parked branches. On `port/vat-rate-kind-s2-open` it is **byte-identical to prod `origin/main`'s** (verified by `diff`; empty). The domain type ported cleanly and needs no adaptation. |
| guard | `issue_preflight.rs:1024` (parked) | `MixedVatRateKindsUnsupported`, justified at `:1267` by *"the summary emitter is single-bucket"* — a justification this ADR falsifies. |
| B4 substrate | `nav_xml.rs:337` (parked) | `pub fn validate_community_vat_number(input: &str) -> Result<(), String>` — normalises into a local and **discards it**. The prod pre-0103 defect, ported verbatim. |

**Teacher-doc check (prod §9(b)).** The prod root-cause document — `docs/walkthroughs/nav-test-hardening-and-vat-walkthrough.md`, which taught the single-bucket collapse as a NAV fact — **does not exist on the Defense line.** `docs/walkthroughs/` at `f7c216e` holds only `defense-workflow.md`, `dr-playbook.md`, `end-to-end-auto-quote-test.md`, `quote-workflow.md`. No cross-branch correction is owed here. The two in-repo teachers that *do* arrive with the merge — the parked `write_summary` comment and the `issue_preflight.rs` guard comment — are rewritten in the same steps that falsify them (§4.1, §4.4).

---

## 3. Port strategy — merge the machinery, hand-apply the corrections

The parked work is ~2 500 lines across 47 files, and `f7c216e` has touched only 3 of them since the fork point. Re-deriving it by hand would be strictly worse than merging it. So:

1. **Merge `port/vat-rate-kind-s2-open`** (which contains `port/vat-rate-kind-s1-machinery` as an ancestor, and `port/vat-rate-kind-eu-partner-plan` as *its* ancestor — one merge covers all three parked refs) into a branch off `f7c216e`. This brings `adr/0101`, `adr/0102`, `vat_rate_kind.rs`, the per-line `vat_rate_kind` field, the DuckDB column, the preflight matrix, the EU-partner customer type and the SPA wiring.
2. **Resolve the single `nav_xml.rs` conflict toward the widened form** (§4.1) — never toward the parked side.
3. **Then apply the prod-line ADR-0103 corrections on top** as clean patches: V (§4.2), I (§4.3), the guard re-founding (§4.4).

Step 3 is *after* the merge because the parked branch delivers the pre-0103 form of each site; patching before merging would just be re-clobbered.

---

## 4. The corrections

### 4.1 — Invariant S, widened · mixed-*kind* bucketing

> **INVARIANT S — Summary coverage.** *For every emitted invoice, the multiset of `(vat_rate_kind, vat_rate_basis_points)` over `summaryByVatRate` buckets equals the distinct set over the lines; and for each bucket, `vatRateNetAmount` / `vatRateVatAmount` / `vatRateGrossAmount` are the sums over **exactly** the lines in that group — no line contributes to a bucket it is not in, and no line contributes to zero buckets.*

`f7c216e` satisfies this invariant's *rate* projection only. The widening:

- `VatRateBucket` gains `kind: VatRateKind`;
- the group key becomes `(line.vat_rate_kind, line.vat_rate_basis_points)`;
- the stable sort becomes `kind.as_str()` then `basis_points` — matching prod exactly, so the two lines emit byte-identical bucket order;
- the conflict at `:1964` resolves to `write_vat_rate(w, b.kind, b.basis_points)` — **the bucket's own** kind and rate, never `lines.first()`'s.

Everything else in `f7c216e`'s `write_summary` — the accumulation loop, the per-bucket `huf_equivalent_for`, the invoice-level sums-over-buckets — is **kept unchanged**. That code is already correct and already pinned.

**Same-rate-different-kind splits.** A 27 % taxable line beside a 27 %-rated domestic-reverse-charge line yields **two** buckets under the widened key and **one** under `f7c216e`'s. This is the concrete defect this ADR closes on the Defense line.

**The four 0 %-kinds each emit their own category** — all four share `basis_points == 0` (forced by ADR-0101 §4) and so are *indistinguishable* under the rate-only key: AAM and KBAET → `vatExemption` (cases `AAM` / `KBAET`), EUFAD37 → `vatOutOfScope`, §142 → `vatDomesticReverseCharge` (boolean, no case code). Under the rate-only key an AAM line and a KBAET line collapse into one bucket that can carry only one case code — silently filing one of them under the other's exemption. **This is the sharpest expression of why the key must widen: at 0 % the rate carries no information at all.**

**Back-compat is exact.** Single-kind invoices — every invoice issuable on the Defense line today, since the kind machinery is parked — partition identically and emit identical bytes. Pinned at T5.

### 4.2 — Invariant V · kind-consistent VAT (B2)

> **INVARIANT V.** *A line whose `vat_rate_kind` is not `Percent` has `vat_amount() == 0`, unconditionally, for every value of `vat_rate_basis_points`.*

`LineItem::vat_amount()` early-returns `Huf::ZERO` when `!self.vat_rate_kind.is_percent()`. `gross_total()` needs no change — it composes, so `gross == net` falls out for exempt/reverse-charge lines.

**V must precede S's widening in landing order** (§5): the widened buckets sum `vat_amount()` per bucket, so with V unfixed a non-`Percent` bucket would carry a non-zero `vatRateVatAmount` — the correct structure faithfully transporting a wrong number.

Placed at the **derivation**, not the gate, deliberately: preflight already carries ADR-0101 §4's `NonZeroPercentForExemptKind`, but preflight is a gate and Invariant P is *deferred on both lines* (§6.1) — so gates are known-bypassable here. Every emit path goes through `vat_amount()`.

### 4.3 — Invariant I · community-VAT validate/emit identity (B4)

> **INVARIANT I.** *For every field both validated and transmitted, the bytes validated, persisted to the side-store, stamped on the audit payload, and written to the NAV XML are the same bytes.*

**Normalise once, at ingest** — not at emit. Emitting normalised would fix gate-vs-wire and leave the side-store and audit payload holding the raw string, relocating the defect rather than closing it (ADR-0102 §3.2 snapshots the number onto all three artifacts).

1. `validate_community_vat_number` returns `Result<String, String>` — the normalisation at `nav_xml.rs:338-342` already exists and is simply thrown away.
2. The ingest boundary writes the normalised value back into `CustomerJson.community_vat_number` before the body is persisted or rendered.
3. `write_customer` is **unchanged** — it keeps emitting the stored field verbatim, which is now normalised by construction.

**Malformed input is still rejected at preflight — no new hole.** The `Err` arm is untouched: the signature change moves what the `Ok` arm *carries*, never what the function *accepts*. Pinned at T9: a malformed number is still a preflight rejection after the change.

### 4.4 — `MixedVatRateKindsUnsupported`: kept, re-founded

The brief asks whether the guard is obsolete, still needed, or now wrong once bucketing is correct. **All three, in sequence: its stated reason becomes wrong, and a different reason keeps it needed.**

Its justification on the parked branch — *"the summary emitter is single-bucket"* — is **falsified** by §4.1. Left in place it would be a comment asserting a closed defect is open, which is exactly how the prod line's B3′ survived.

But a second, independent and stronger reason stands: **ADR-0102 §4(a)'s buyer-status ↔ line-kind matrix is invoice-scoped.** `IntraCommunityGoods` requires `customerVatStatus = Other`; `DomesticReverseCharge` requires `Domestic`. An invoice carrying one line of each demands both simultaneously — unsatisfiable. Post-widening such an invoice would produce two structurally well-formed buckets and pass the local validator while being semantically impossible.

**Decision: keep the guard, rewrite its justification wholesale, pin the behaviour (T8).** This is not defence-in-depth — it is a different invariant that happened to share an implementation. Conservative, and it matches prod, which reached the same conclusion against its author's initial position.

---

## 5. Ordering

```
merge machinery ──▶ V (B2) ──▶ S-widening ──▶ guard re-founding ──▶ I (B4)
```

V before S is a **real** dependency (§4.2). I is independent and lands last only because it is the most isolated. The guard re-founding rides with S because S is what falsifies its comment.

---

## 6. Scope — what this ADR does NOT do

### 6.1 Deferred, matching prod

**Invariant P (B1a — preflight universality)** and **Invariant C (B1b — chain congruence)** are **not in this change.** They are deferred on the prod line too (prod §11), so porting them here would put Defense *ahead* of prod on the highest-blast-radius change in the ADR — 1-of-8 → 8-of-8 doors, with the §3.4 fixture fallout — while the pilot has no real machines or customers to justify that risk. **Conservative option, explicitly flagged as the brief requires.**

The residual is stated plainly, not softened: **a preflight-bypassing CLI door can still originate an ungated body.** After this change it will emit a *correct summary* and *kind-consistent VAT* for whatever lines it carries — S and V sit at emit and derivation, below the gate — but it is not gated. That is unchanged from today and identical to prod's posture.

### 6.2 Other residuals

1. **`vat_rate_basis_points` stays readable on non-`Percent` lines.** V closes every consumer that exists; the structural fix (rate carried only in the `Percent` variant) is a type-level refactor across the domain, the DuckDB adapter, the ADR-0049 side-store shape and the SPA wire. Deferred as its own ADR. Named, not dropped.
2. **NAV `maxOccurs="unbounded"` on `summaryByVatRate`** is inherited from prod's §10.1, where it was confirmed against the published `invoiceData.xsd`. The local validator is deliberately loose (ADR-0022) and its acceptance is **not** treated as confirmation.
3. **ADR-0102's implementation status.** Defense ADR-0102 is landed design-only. This change lands the B4 slice of that surface (community-VAT identity) because the parked branch already carries the machinery; it does not claim ADR-0102 is fully implemented on Defense.

---

## 7. Proof — the test plan (every pin mutation-verified)

**Standing rule: reverting the fix must make the test fail.** A pin whose mutation was not run is not landed. Each row names its mutation; §8 records the observed red.

| Pin | Test | Mutation that MUST turn it red |
|---|---|---|
| **T1** | **⭐ Mixed *kind*, same rate** — a 27 % `Percent` line beside a 27 % `DomesticReverseCharge` line: assert **two** buckets, each with its own choice element (`vatPercentage` vs `vatDomesticReverseCharge`), each carrying only its own line's money. **This is the defect the rate-only key cannot see.** | revert the bucket key to `basis_points` only |
| **T2** | **⭐ Two 0 %-kinds on one invoice** — AAM + EUFAD37, both `basis_points == 0`: two buckets, `vatExemption`/case `AAM` and `vatOutOfScope`/case `EUFAD37`. Under the rate-only key these collapse into one. | same |
| **T3** | The four 0 %-kinds each map to the right category, asserted field by field on the emitted XML. | break any one `vat_rate_choice` arm |
| **T4** | **B3′ regression pin stays green** — mixed *rate* (27 % + 5 %, all `Percent`) still yields two buckets with correct per-bucket money. *This is `nav_xml_summary_multibucket.rs`, inherited from `f7c216e` and not modified.* | collapse the emitter to `lines.first()` — proves the landed fix cannot return |
| **T5** | **Golden single-bucket back-compat**: a single-rate single-kind invoice emits byte-identically to `f7c216e`. | any change to the single-group path |
| **T6** | Deterministic order across line permutations, incl. two kinds at the same rate (which `sort_by_key(basis_points)` alone leaves order-dependent). | drop the `kind.as_str()` sort term |
| **T7** | `vat_amount()` is 0 for each non-`Percent` kind **with a deliberately non-zero `basis_points`** — the state an ungated door admits. | revert `vat_amount` to rate-only |
| **T8** | Mixed-kind guard still rejects `KBAET + DRC` **after** widening — the §4.4 re-founding verified behaviourally, not just re-commented. | delete the guard |
| **T9** | Community VAT round-trip: `"at u123 45678"` is validated, persisted, audited and emitted as one identical normalised string (**four-way** assertion); and a malformed number is still preflight-rejected. | revert to discarding the normalised value |
| **T10** | Per-bucket HUF (ADR-0037 §1.c) on a mixed-**kind** EUR invoice: invoice-level HUF equals the **sum of per-bucket** HUF, not a fresh conversion of the grand total. | convert the grand total instead |

⚠ **T10 fixture constraint:** `huf_equivalent_for` is the identity for `Currency::Huf`, so a HUF-only fixture cannot pin any HUF-conversion behaviour. T10 uses **EUR + `RateMetadata`** with per-bucket amounts chosen so sum-of-conversions ≠ conversion-of-sum.

**Gates.** Both edition arms green — Portable and Defense `--features production` — plus `cargo fmt`, build, test, `clippy -D warnings`, and the standing cut-gate/census checks, on every step.

---

## 8. Implementation record

*(filled in as each step lands — a step is not landed until its mutations are observed red)*

- [ ] Step 0 — merge machinery, resolve the `nav_xml.rs` conflict toward the widened form
- [ ] Step 1 — V (B2)
- [ ] Step 2 — S widening + guard re-founding
- [ ] Step 3 — I (B4)
- [ ] Step 4 — pins T1–T10, each mutation-verified
