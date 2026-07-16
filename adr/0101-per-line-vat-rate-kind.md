# ADR-0101 (Defense) — Per-line VAT rate-kind: issuing & submitting the NAV 0%/exempt VAT sub-types (port of ABERP.git ADR-0101)

- **Status:** **Accepted — ported to Defense (ABERP-Editions), Session 1 landed 2026-07-16.** This is the Defense-line port of Portable **ABERP.git ADR-0101** (shipped there as `PROD_v2.32.0` @ `bb381a2`). The design below is inherited verbatim from Portable (§0–§10); this preamble records the **Defense-specific port decisions and the one forced deviation**.
- **Date:** 2026-07-16
- **Deciders:** Ervin Áben (greenlit the VAT feature "as many sessions as we need"). Port design + implementation by Dispatch.
- **Port provenance:** Portable feature range `d2d60ef..bb381a2` on `ABERP.git`; the compliance core (`nav_xml.rs`, `issue_preflight.rs`, `nav-xsd-validator/validate.rs`, `modules/billing` domain) was **byte-identical** to the shared frozen-prod base, so the machinery applies as a clean patch onto Defense. See `docs/PORT-PLAN-vat-rate-kind-eu-partner.md`.
- **Governance:** this repo has **no `CLAUDE.md`**; the equivalent Defense obligations live in `SAW-OFF.md` (ADR-0093 saw-off + "DO NOT TOUCH PROD"), `FOUNDATION.md`, and the `adr/` series through ADR-0099 (plus the DB-isolation **cut-gate**, `tools/cut_gate_db_isolation.sh`). The "CLAUDE.md rule N" references in the inherited design (rules 8/11/12/…) are Portable-repo conventions; read them as the Defense analogues in those docs.

---

## D. Defense port decisions & deviations (read first)

### D.1 ADR numbering — mirror, not contiguous (⚑ FLAG — Ervin can override)
Defense's ADR log is independent and ends at **ADR-0099**; **ADR-0100 is free**. Dispatch chose to **mirror the Portable numbers** — Defense `ADR-0101` (this) + `ADR-0102` (EU-partner) — so the compliance story is traceable by number across both repos. **This deliberately leaves Defense ADR-0100 as a gap.** The contiguous alternative (Defense 0100 + 0101, no gap, but numbers no longer line up with Portable) was rejected for traceability. ⚑ **Ervin may override to contiguous**; if so, renumber both files + this cross-reference.

### D.2 [MUST] `read_base_line_vat_kinds` rides the shared `aberp_db::Handle` — the single forced code deviation
Portable's modification silent-downgrade guard (its ADR-0101 S2, commit `d91d0ee`) reads the base invoice's per-line kinds via its **own** `Connection::open(db_path)` (Portable opener #91). **That is illegal on Defense:** ADR-0098/0099 drove every in-process invoicing/serve opener onto the ONE shared `aberp_db::Handle`, and the cut-gate (CHECK 10f/10h) **bans** a fresh `Connection::open` in the serve request surface. The Defense port therefore **re-implements** the reader as:

```rust
fn read_base_line_vat_kinds(db: &aberp_db::HandleArc, invoice_id: &str)
    -> Result<Vec<aberp_billing::VatRateKind>>
{
    let mut conn = db.read().context("shared read: base vat_rate_kind lookup (ADR-0098 Gap 1a Session C)")?;
    // … same scoped read-tx + billing::load_ready_invoice_by_id body as Portable …
}
```

riding `db.read()` exactly like the sibling `read_invoice_total_gross_minor` / `read_base_currency` in this repo's `serve.rs`. The guard call site in `modification_invoice_request` passes `&state.db` (already held) instead of `&state.db_path`. The SQL body / `load_ready_invoice_by_id` reuse and the guard logic are **unchanged**.

### D.3 [MUST-VERIFY] Census obligation INVERTS vs Portable — prove ZERO opener churn
Because the Defense reader rides the already-open shared Handle, it adds **zero** new openers. So — unlike Portable, which *registered a new baseline* (its opener-fingerprints 28→29, census 90→91) — the Defense port must prove the opposite: **no opener/residual census file changes at all.** Session 1 verified `tools/adr0098_r4_opener_fingerprints.txt`, `tools/adr0099_write_fork_residuals.txt`, `tools/adr0098_c2_frozen_residuals.txt` (and the scanners) are **byte-identical** before/after the port, and the full cut-gate (CHECK 10f/10g/10h/10i/10k/10M) stays green. **If any opener count moves, the reader is not on the Handle — stop and fix.**

### D.4 [TRIVIAL] `issue_invoice.rs` import re-anchor
Defense restructured the `issue_invoice.rs` import block (ADR-0098), so Portable's import hunk did not context-match. The actual change is one token — adding `VatRateKind` to the `aberp_billing::{…}` use-list — hand-applied; the deeper `LineJson.vat_rate_kind` / `build_command` hunks land on stable regions unchanged.

### D.5 Session-1 scope on Defense (machinery behind a shut door)
Session 1 landed: the `VatRateKind` closed vocab; the `LineJson`/`LineItem` field; the `invoice_line.vat_rate_kind` column + additive DuckDB-safe migration; the `nav_xml.rs` emit (line + `summaryByVatRate` mirror); the `nav-xsd-validator` extension; the Handle-routed `read_base_line_vat_kinds` + modification guard (D.2); and the S1 test corpus. **Preflight STILL REJECTS every non-`Percent` kind** (`LineItemVatRateKindNotYetIssuable`) — the door stays shut, zero prod ÁFA exposure. Opening the preflight matrix, the EU-partner buyer type (ADR-0102), and the SPA selector are **later Defense port sessions**. The NAV-category adversarial review vs the authoritative NAV OSA 3.0 XSD is a held pre-cut Defense session (port plan §5). Cut target: **`PROD_Defense_v0.3.0`** (held for Ervin — Defense cuts are gates-green-only per pilot posture).

### D.6 Migration composition (verified)
The `invoice_line.vat_rate_kind` column add slots into Defense's own migration ladder (after `MIGRATE_S157_SQL`) as `MIGRATE_ADR0101_SQL` — idempotent `ADD COLUMN IF NOT EXISTS` (nullable) + `UPDATE … WHERE vat_rate_kind IS NULL` backfill to `'Percent'`, DuckDB-v1-safe (no inline constraint on ALTER). Every existing row hydrates as `Percent` → byte-identical NAV bytes.

---

## Inherited Portable design (verbatim from ABERP.git ADR-0101 @ bb381a2)

> The sections below are the authoritative feature design, carried verbatim for the cross-repo compliance record. File:line references are Portable's; because the compliance-core files are byte-identical on Defense they resolve to the same code here, **except** `serve.rs` (see D.2). Portable's "Session 1/2/3" sequencing (§9) is Portable's cadence; the Defense port cadence is in `docs/PORT-PLAN-vat-rate-kind-eu-partner.md` §5 and D.5 above.


- **Status:** **Proposed** (design-only, 2026-07-15). No application code written in this session. Ervin greenlit building this properly ("as many sessions as we need"); this ADR is the design the implementation sessions execute against.
- **Date:** 2026-07-15
- **Deciders:** Ervin Áben (greenlit the feature). Design-pass by Dispatch.
- **Supersedes / closes:** the two `reports.rs:48-51` deferrals for the 0%-sub-type report breakout (Part 1 here); the ADR-0038 §"Open questions" named-deferral of AAM/TAM/TAH; and the `apps/aberp/src/issue_preflight.rs:53-57` "different wire-shape surface … named-deferred" note. Extends **ADR-0048** (customer VAT status closed-vocab; this ADR is the *line*-level analogue — same closed-vocab + named-deferral pattern).
- **Related:** ADR-0022 (NAV XSD runtime validator — `crates/nav-xsd-validator`), ADR-0037/0044γ (invoice_line schema + migration ladder), ADR-0042 (invoice notes NEVER in NAV XML — the PII firewall this ADR must not breach), ADR-0043 (invoice date rules), ADR-0049 (storno/modification chain — replays side-stored `input.json`, so `LineJson` shape is chain-load-bearing). NAV wire-shape gotchas: `reference-nav-gotchas` memory §1–§4.
- **Base finding:** `docs/VAT_0PCT_SUBTYPE_REPORTING_FINDING.md` on branch `vat-0pct-subtypes-reporting` @ 35360c6 (grep-verified current-state; this ADR is its "Recommended design", promoted and made authoritative).

---

## 0. TL;DR

ABERP today can only emit a **numeric** VAT rate per line: `<lineVatRate><vatPercentage>{rate}</vatPercentage></lineVatRate>` (`nav_xml.rs:1554-1560`). A "0% line" is therefore a literal `vatPercentage 0.00`. That is **wrong** for the Hungarian zero-VAT situations, which are *not* a numeric zero rate but distinct NAV VAT-category choices — **AAM** (alanyi adómentesség), **belföldi fordított adózás** (domestic reverse-charge), and the **EU / intra-Community** exempt/out-of-scope cases. Preflight (`issue_preflight.rs:57,631`) actively **rejects** them ("Speciális kategóriák (AAM/TAM/TAH) jelenleg nem támogatottak").

This ADR adds a **closed-vocab per-line discriminant `vat_rate_kind`** (default `Percent` → 100% backward-compatible), threads it through the model → persisted schema (+ additive migration) → preflight → NAV emit → local XSD validator, and — once lines carry it — makes the `reports.rs` 0%-sub-bucket breakout a trivial GROUP-BY.

**The compliance crux is the NAV mapping (§2).** The repo vendors **no** NAV `invoiceApi.xsd` file and **no** NAV code-list — the "vendored schema" is the hand-written Rust model in `crates/nav-xsd-validator/src/validate.rs`, which models `lineVatRate` **loosely** (4 choice members, inner children `skip_to_matching_end`-skipped, **zero** case codes). **Therefore every NAV case code in §2 is FLAGGED "⚠ MUST confirm against the published NAV OSA 3.0 code list before implementation."** No code in this repo authoritatively pins them, and per the task constraint I do **not** invent or guess NAV codes — the values below are best-knowledge starting points for confirmation, not repo-verified facts.

---

## 1. Current state (grep-verified)

| Layer | Today | File:line |
|---|---|---|
| Input model `LineJson` | `vat_rate_percent: u16` only — no category field | `apps/aberp/src/issue_invoice.rs:288-289` |
| Preflight vocab | `ALLOWED_VAT_RATES_PERCENT = [0,5,18,27]`; AAM/TAM/TAH explicitly rejected | `apps/aberp/src/issue_preflight.rs:57`, reject at `:631`, msg `:314`/`:397` |
| Persisted schema | `invoice_line.vat_rate_basis_points INTEGER` — no category column | `modules/billing/src/adapters/duckdb_store.rs:177-195` |
| NAV emit | `write_line_vat_rate` **always** `<vatPercentage>{rate}</vatPercentage>` | `apps/aberp/src/nav_xml.rs:1554-1560` |
| Local validator | `lineVatRate` ALLOWED = `{vatPercentage, vatContent, vatExemption, vatOutOfScope}`; only `vatPercentage` numeric-checked, the rest **skipped**; **no** `vatDomesticReverseCharge` | `crates/nav-xsd-validator/src/validate.rs:914-953` (and the mirror `walk_vat_rate` for `summaryByVatRate` at `:957`) |
| Report | AAM / reverse-charge / EU-0 all lumped as `0%`; deferral admits "schema does NOT distinguish them" | `apps/aberp/src/reports.rs:48-51` |

**Key correction to the deferral comment:** `reports.rs:50` ("Parsing the NAV XML to recover the tag") is mistaken — the emitted XML carries **no** sub-type tag to recover (it hardcodes `vatPercentage 0.00`). The discriminator has to be **captured at issuance**, which is exactly what this ADR does.

---

## 2. THE CRUX — NAV OSA 3.0 `lineVatRate` mapping (⚠ codes require confirmation)

### 2.1 Provenance caveat (read first)

The repo does **not** contain the authoritative NAV artifact. Confirmed by search: **zero `.xsd` files** in-tree; the only NAV schema is `crates/nav-xsd-validator/src/validate.rs`, a deliberately-loose hand model (ADR-0022) that `skip_to_matching_end`s the interior of `vatExemption`/`vatOutOfScope` and does not know `vatDomesticReverseCharge`, `case`, or `reason`. **No NAV *case code* is grep-findable anywhere in the repo.**

Consequently the case codes and element names below are drawn from published-NAV knowledge, **not** from a repo artifact, and each is flagged **⚠ CONFIRM**. Before the implementation session that touches `nav_xml.rs`, confirm every ⚠ row against the published NAV OSA 3.0 `invoiceApi.xsd` + the NAV *Adómentesség jelölés* / *ÁFA tárgyi hatályán kívüli* code lists (canonical source: `github.com/nav-gov-hu/Online-Invoice` → `src/schemas/nav/gov/hu/OSA/invoiceApi.xsd`, `LineVatRateType`). **Do not ship any ⚠ row unconfirmed** — the first prod submission counts toward real ÁFA; there are no smoke tests.

### 2.2 The FULL `LineVatRateType` choice group

NAV's `lineVatRate` is an XSD **choice** — **exactly one** member per line. Complete member list (for a complete `vat_rate_kind` vocab):

| # | Choice element | Shape | Meaning | In repo validator? |
|---|---|---|---|---|
| 1 | `vatPercentage` | decimal | Numeric rate (27/18/5/0) | ✅ modeled + numeric-checked |
| 2 | `vatContent` | decimal | Gross-inclusive áfatartalom (simplified/retail) | ✅ in ALLOWED, skipped |
| 3 | `vatExemption` | `{case, reason}` | Adómentes (exempt) | ✅ in ALLOWED, interior **skipped** |
| 4 | `vatOutOfScope` | `{case, reason}` | ÁFA tárgyi hatályán kívül (out of scope) | ✅ in ALLOWED, interior **skipped** |
| 5 | `vatDomesticReverseCharge` | boolean `true` | Belföldi fordított adózás | ❌ **NOT modeled — must add** |
| 6 | `marginSchemeIndicator` | enum | Különbözet szerinti (margin scheme) | ❌ not modeled |
| 7 | `vatAmountMismatch` | `{vatRate, case}` | Eltérő áfa-tartalom | ❌ not modeled |
| 8 | `noVatCharge` | boolean `true` | Nincs felszámított áfa (§17 / v3.0 addition) | ❌ not modeled |

⚠ CONFIRM the exact set (member #6-#8 names + presence) against the published `LineVatRateType` — the NAV v3.0 minor revisions have added members over time.

### 2.3 The three requested kinds → exact emit

| Requested kind | NAV choice element | case code | reason (free-text, non-blank) | Statute |
|---|---|---|---|---|
| **AAM** — alanyi adómentesség | `vatExemption` | **`AAM`** ⚠ CONFIRM | e.g. `"Alanyi adómentesség"` | Áfa tv. §187-196 |
| **Domestic reverse-charge** — belföldi fordított adózás | `vatDomesticReverseCharge` (bool `true`) | *(no case code — boolean element)* ⚠ CONFIRM element name | — | Áfa tv. §142 |
| **EU / intra-Community 0%** — **NOT one kind** (see §2.4) | split → `vatExemption` **or** `vatOutOfScope` | `KBAET` / `EUFAD37` ⚠ CONFIRM | statutory ref | §89 / §37 |

### 2.4 "EU / intra-Community 0%" is TWO different NAV shapes — the load-bearing subtlety

There is no single "EU 0%" element. The correct NAV shape depends on **goods vs cross-border service**:

- **Intra-Community exempt supply of _goods_** (termékértékesítés, §89) → `vatExemption` case **`KBAET`** ⚠ CONFIRM. (New means of transport, §89(2), is a *different* code **`KBAUK`** ⚠ CONFIRM.)
- **Cross-border _service_ reverse-charged at the customer's member state** (§37) → `vatOutOfScope` case **`EUFAD37`** ⚠ CONFIRM — it is *out of HU scope* because the place of supply is the other member state, **not** an exemption.

**Design consequence:** the vocab must expose these as **two** kinds (`IntraCommunityGoods` vs `IntraCommunityServiceReverse`), not one. Collapsing them would force one of the two into the wrong NAV element — a compliance error that passes the loose local validator but is wrong to NAV. This is exactly the class of "looks authoritative but wrong" the base finding warned about.

### 2.5 Reference code lists (⚠ ALL CONFIRM — none in repo)

For completeness of the closed vocab, the published `vatExemption/case` and `vatOutOfScope/case` code lists (confirm exact membership + spelling):

- **`vatExemption/case`** (Adómentesség jelölés): `AAM`, `TAM`, `KBAET`, `KBAUK`, `EAM`, `NAM`, `UNKNOWN` ⚠
- **`vatOutOfScope/case`** (tárgyi hatályán kívül): `ATK`, `EUFAD37`, `EUFADE`, `EUE`, `HO`, `UNKNOWN` ⚠

`reason` is a non-blank free-text string (`SimpleText…` type) — structurally any non-blank text passes NAV; the statutory reference strings above are recommended defaults, operator-confirmable. **The load-bearing, must-confirm field is the `case` code, not the `reason`.**

---

## 3. Design — `vat_rate_kind` closed vocab

### 3.1 The enum (Rust)

```
enum VatRateKind {
    Percent,                        // default — vatPercentage (UNCHANGED path)
    AamExempt,                      // vatExemption / case=AAM
    DomesticReverseCharge,          // vatDomesticReverseCharge=true
    IntraCommunityGoods,            // vatExemption / case=KBAET
    IntraCommunityServiceReverse,   // vatOutOfScope / case=EUFAD37
    // ── named-deferred (explicit not-yet markers, ADR-0048 pattern) ──
    // TamExempt, ExportGoods(EAM), OtherIntl(NAM), NewTransport(KBAUK),
    // OutOfScopeThirdCountry(HO), MarginScheme, NoVatCharge, VatContent …
}
```

**v1 fully-wires the 4 non-Percent kinds the task named** (AAM, domestic reverse-charge, and the two intra-Community cases). The remainder of the choice group (§2.2 / §2.5) is **named-deferred** exactly like ADR-0048 deferred `Other` — the enum knows the names, but preflight rejects them and emit `anyhow!`s them, as explicit "not yet" markers (CLAUDE.md rule 12). This keeps v1 minimal (rule 2) while the vocab is *closed* and complete-in-intent.

### 3.2 case + reason are DERIVED, not stored (CLAUDE.md rule 5)

Each fully-wired kind maps to **exactly one** `(element, case, reason)` triple. That mapping is a **single code table in `nav_xml.rs`** (one source of truth), consumed by both emit and preflight. **No new free-text column in v1** — the model is a *single* enum column. If operators later need custom reason wording, add a nullable `vat_exemption_reason VARCHAR` (named-deferred; not v1).

> Rationale: code answers a deterministic transform; the model carries only the judgment call (which kind). Storing case/reason per line would duplicate the derivable and risk drift between stored reason and statutory truth.

### 3.3 Placement (per-layer)

| Layer | Change | File |
|---|---|---|
| **Input model** | add `#[serde(default)] vat_rate_kind: VatRateKind` to `LineJson` (default `Percent` so pre-existing side-stored `input.json` bodies — replayed by storno/modification per ADR-0049 — deserialize as `Percent` and round-trip byte-identically) | `apps/aberp/src/issue_invoice.rs:277-306` |
| **Persisted schema** | add column `vat_rate_kind VARCHAR NOT NULL DEFAULT 'Percent'` to `invoice_line`; writer persists it, reader hydrates it | `modules/billing/src/adapters/duckdb_store.rs:177-195` |
| **Migration** | additive ladder step `MIGRATE_S<n>_SQL = "ALTER TABLE invoice_line ADD COLUMN IF NOT EXISTS vat_rate_kind VARCHAR NOT NULL DEFAULT 'Percent';"` — existing rows backfill to `Percent`; register in the ladder next to `MIGRATE_S157_SQL` (`duckdb_store.rs:860`) | `modules/billing/src/adapters/duckdb_store.rs:343-347,854-860` |
| **Preflight** | accept the 4 new kinds with per-kind rules (§4); update/remove the "AAM/TAM/TAH not supported" reject text | `apps/aberp/src/issue_preflight.rs:57,314,397,631` |
| **NAV emit** | `write_line_vat_rate` branches on kind → renders the correct choice element (§2.3) instead of flat `vatPercentage 0.00` | `apps/aberp/src/nav_xml.rs:1554-1560` |
| **Validator** | add `vatDomesticReverseCharge` to `lineVatRate` ALLOWED; (should-harden) model `case`+`reason` children of `vatExemption`/`vatOutOfScope`; keep exactly-one-choice check | `crates/nav-xsd-validator/src/validate.rs:914-953` |
| **Report** | GROUP BY `vat_rate_kind` → 0%-sub-buckets | `apps/aberp/src/reports.rs:48-51` |

### 3.4 The vatPercentage/vatContent summary mirror

`nav_xml.rs::write_vat_rate` (`:1546-1552`) and validator `walk_vat_rate` (`:957`) handle `summaryByVatRate/vatRate` — the **invoice-level** aggregate, same choice shape. NAV requires the summary's `vatRate` to **match** the lines' rate categories. So a non-Percent line must also produce the matching **`summaryByVatRate`** entry (e.g. an AAM line → a summary bucket keyed on `vatExemption/AAM`, not `vatPercentage 0.00`). **This is in-scope for the NAV-emit session** — emitting the line correctly but leaving the summary as `vatPercentage 0.00` would be a NAV cross-field mismatch rejection. Grep `write_summary`/`summaryByVatRate` in `nav_xml.rs` before that session; the round-trip test must assert line **and** summary agree.

---

## 4. Preflight accept/reject matrix

| kind | vat_rate_percent | verdict |
|---|---|---|
| `Percent` | ∈ {0,5,18,27} | ✅ accept (unchanged) |
| `Percent` | else | ❌ `LineItemVatRateUnknown` (unchanged) |
| `AamExempt` / `DomesticReverseCharge` / `IntraCommunityGoods` / `IntraCommunityServiceReverse` | `0` | ✅ accept |
| any non-Percent kind | ≠ 0 | ❌ new error `NonZeroPercentForExemptKind` (a reverse-charge/exempt line MUST be 0%) |
| named-deferred kind (TAM/EAM/…) | any | ❌ `VatRateKindNotSupportedYet` (explicit not-yet, ADR-0048 pattern) |
| unknown kind string (deserialization) | — | serde rejects closed vocab → 400 |

Line VAT **amount** for every non-Percent kind is **0** (`lineVatData` = 0; buyer self-assesses for reverse-charge). Preflight/compute must force it, not trust the caller.

---

## 5. Backward compatibility (HARD invariant)

1. **`Percent` is the default at every layer** — model `#[serde(default)]`, column `DEFAULT 'Percent'`, migration backfill. A pre-ADR-0101 body with no `vat_rate_kind` deserializes, persists, and emits **byte-identically** to today.
2. **Emit is branch-guarded:** `Percent` takes the *exact existing* `write_line_vat_rate` code path (`<vatPercentage>{rate}</vatPercentage>`). Only a non-`Percent` line hits new emit code. Every existing 0%-percent invoice and every existing on-disk NAV submission is therefore unchanged.
3. **No retroactive re-emit.** Issued invoices' side-stored `input.json` + on-disk NAV XML are never rewritten (consistent with `reference-nav-gotchas §3` — the on-disk XML is the canonical record of what NAV saw). Storno/modification of a *pre-0101* invoice replays `Percent` and reproduces the original bytes.
4. **Regression pin:** a golden test asserts a `Percent`/0% line emits `vatPercentage 0.00` (byte-for-byte the pre-0101 output) — this test must be **impossible to pass if the default path changed** (CLAUDE.md rule 9).

---

## 6. Report breakout (Part 1 of the base finding — closed)

Once `invoice_line.vat_rate_kind` exists, the `reports.rs:48-51` deferral collapses to a **GROUP BY vat_rate_kind** over issued lines, surfacing distinct buckets: `Percent(0)`, `AamExempt`, `DomesticReverseCharge`, `IntraCommunityGoods`, `IntraCommunityServiceReverse`. No XML parsing, no derivation — the discriminator is a first-class column. Delete the deferral comment as part of this step.

## 7. Out of scope — Part 2 follow-up (per-rate for restored/incoming)

The base finding's Part 2 — per-VAT-rate breakdown for **restored/incoming** invoices — stays **out of scope** here. `restored_invoice` (`restore_from_nav_outgoing.rs:192-211`) and `ap_invoice` (`incoming_invoices.rs:383-408`) are `queryInvoiceDigest`-derived (invoice-level totals only). A per-rate breakdown needs re-ingesting `queryInvoiceData` (full base64 invoice XML), parsing `summaryByVatRate`, and persisting a new per-rate child table — a NAV-transport + schema-migration change. **Separate ADR/PR.** Do not fabricate an "effective rate" from `vat/net` (blends mixed-rate invoices — CLAUDE.md rules 9+11).

---

## 8. Test plan

**A. NAV round-trip per kind** (the compliance gate). For each fully-wired kind: build a line → assert emitted `lineVatRate` XML is the exact expected choice element + case + reason (§2.3) → feed through `nav-xsd-validator` → passes → the emitted structure matches the (confirmed) published `LineVatRateType`. Include the `summaryByVatRate` mirror (§3.4): line kind and summary bucket agree. Negative pins: AAM line does **not** emit `vatPercentage`; reverse-charge line emits `vatDomesticReverseCharge` and **not** `vatExemption`.

**B. Backward-compat (byte-identical).** Golden: a `Percent`/0% line emits `vatPercentage 0.00` byte-for-byte vs the pre-0101 snapshot. A pre-0101 side-stored `input.json` (no `vat_rate_kind`) round-trips through storno/modification unchanged. Migration on a pre-0101 DB backfills every row to `Percent` and no emitted byte changes.

**C. Preflight accept/reject matrix** (§4) — one test per row, including `NonZeroPercentForExemptKind`, `VatRateKindNotSupportedYet`, and unknown-string serde rejection.

**D. PII / notes firewall (ADR-0042) still holds.** Assert the per-line `note` still never reaches NAV XML and that `vat_rate_kind`/`reason` introduce **no** buyer-identifying free text onto the wire (reason strings are statutory, not operator-PII). Re-run the existing notes-no-leak pins; add a `reason`-content assertion.

**E. Report GROUP-BY correctness.** Seed lines across all kinds (incl. mixed-kind invoices) → assert each bucket count/sum is exact and mixed invoices are **not** blended into one bucket (rule 11). A bucket total must change when a seeded line's kind changes (rule 9 — the test can fail on logic change).

**F. Validator.** `vatDomesticReverseCharge` now accepted (was `UnexpectedElement`); a malformed exemption (missing `case` — if the should-harden lands) is rejected; exactly-one-choice enforced.

---

## 9. Implementation sequencing (recommended)

Three gate-green sessions (CLAUDE.md rule 4 — `cargo fmt + build + test + clippy -D` + the coherence/regression pins each step). Ordered so the **NAV machinery lands behind a closed door**, and the single risky flip — invoices actually reaching NAV in the new shape — is isolated and adversarially reviewed.

**Session 1 — model + schema + migration + NAV emit + validator, preflight STILL REJECTS.**
Land `VatRateKind`, the `invoice_line` column + migration, the `nav_xml.rs` emit branch (+ summary mirror §3.4), and the validator `vatDomesticReverseCharge` add — **but leave preflight rejecting the new kinds** so no new-kind invoice can be issued yet. Nothing reaches NAV. This satisfies CLAUDE.md rule 14 (writers *and* readers of the wire land together; the door stays shut) and proves kind→XML→validator round-trips (test group A/B/F) with **zero** production exposure. **⚠ Blocked on:** confirming every §2 case code first.

**Session 2 — open preflight (activation) + UI surface. ⭐ ADVERSARIAL NAV-CATEGORY REVIEW BEFORE THIS CUT.**
Flip preflight to accept the 4 kinds (§4 matrix) and expose the kind picker in the SPA/CLI. This is the compliance-load-bearing moment (first real ÁFA submission). Full adversarial NAV review here per CLAUDE.md rule 4's "invoice→NAV/ÁFA path" checkpoint: re-verify every case code against the published code list, the summary-mirror agreement, and the accept/reject matrix. Test group C/D.

**Session 3 — report breakout.** GROUP BY `vat_rate_kind`; delete the `reports.rs` deferral. Test group E. Lowest-risk, ships last.

Rationale for emit-before-preflight-open (not the reverse): opening preflight while emit still rendered `vatPercentage 0.00` would let a "0% AAM" line *issue* and reach NAV as a plain numeric zero — a silent mis-categorization (CLAUDE.md rule 11, fail-loud). Landing emit first behind a shut door removes that window.

---

## 10. Flagged assumptions & open items (conservative choices; no AskUserQuestion)

1. **⚠ ALL NAV case codes unconfirmed against a repo artifact** (§2). The repo vendors no XSD/code-list; §2 values are best-knowledge, not repo-verified. **Every ⚠ row MUST be confirmed against the published NAV OSA 3.0 `invoiceApi.xsd` + code lists before the Session-1 emit lands.** Not guessed here per the task constraint.
2. **"EU/intra-Community 0%" split into two kinds** (goods `KBAET` via `vatExemption` vs cross-border service `EUFAD37` via `vatOutOfScope`) — §2.4. Conservative choice: model both rather than collapse and risk the wrong element. If Ervin only needs one in practice, the other is trivially named-deferred.
3. **case + reason derived in code, not stored** (§3.2) — single enum column, no free-text reason column in v1. If custom reason wording is needed, a nullable column is the named-deferred extension.
4. **Named-deferred remainder** (TAM/EAM/NAM/KBAUK/HO/marginScheme/noVatCharge/vatContent) — enum-known, preflight-rejected, emit-`anyhow!`ed as explicit not-yet markers (ADR-0048 pattern). v1 wires only the task's four.
5. **ADR path deviation:** the task said `docs/adr`; the repo's ADRs live in `adr/` (verified — `docs/adr/` does not exist). Placed at `adr/0101-…` to match convention (CLAUDE.md rule 10). Next free number confirmed 0101 (0100 = SaaS).
6. **Summary-mirror scope** (§3.4): included in Session 1 because a line/summary category mismatch is a NAV rejection; flagged so it is not forgotten as "just the line."
