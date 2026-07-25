# Port plan — VAT rate-kind + EU-partner customer type → Defense (ABERP-Editions)

**Status:** PLAN ONLY (read-only scoping pass). No source changed. No commits beyond this file on branch `port/vat-rate-kind-eu-partner-plan` (not merged, not pushed).
**Author pass date:** 2026-07-16
**Source feature:** Portable `ABERP.git` — shipped as `PROD_v2.32.0` (`bb381a2`), ADR-0101 (per-line `vat_rate_kind`) + ADR-0102 (EU-partner customer type) + ADR-0099 census addendum.
**Target:** this repo, `ABERP-Editions.git`, Defense line, live at `PROD_Defense_v0.2.11`.

> **Note on the STEP-0 instruction.** There is **no `CLAUDE.md`** at this repo root (verified: `find -iname CLAUDE.md` → none). The governing docs are `SAW-OFF.md` (ADR-0093 saw-off contract + cardinal "DO NOT TOUCH PROD" rule), `FOUNDATION.md`, and the `adr/` series through **ADR-0099**. The "CLAUDE.md rule N" references quoted in the Portable code comments (rules 8/11) are Portable-repo conventions; the equivalent Defense obligations live in those ADRs + the cut-gate. Versioning/gates here differ from Portable and are captured in §7.

---

## 1. Repo topology & why this is a PORT, not a merge

- Both lines were sawed off the frozen unified Prod (`main` fork point `2bd2adf`, per ADR-0093). Prod stays frozen at `PROD_v2.27.76`.
- The two repos have **independent git objects** — Portable's feature-base `d2d60ef` and tip `bb381a2` are **ABSENT** in this repo (`git cat-file -t` → absent). The only shared reachable commits are the prod ancestors (`2bd2adf`, `f7519b4`). So there is **no common branch to merge**; every hunk lands as a patch onto diverged code.
- Divergence is **asymmetric**: Defense carried its own strand (digital-ID, firing-sites, heat-lot, NCR-CAPA, part-UID, on-machine-probe, and the big ADR-0095→0099 crash-safe-durability / in-process-opener-consolidation work). Portable carried the invoicing/VAT strand (ADR-0100→0102). **Neither line's ADR numbers overlap the other's.**

**Good news that shapes the whole plan:** the VAT-compliance *core* files never diverged from the shared prod base. Measured drift (Editions HEAD vs Portable feature-base `d2d60ef`, added+removed lines):

| File | Drift | |
|---|---:|---|
| `apps/aberp/src/nav_xml.rs` | **0** | byte-identical to prod base |
| `apps/aberp/src/issue_preflight.rs` | **0** | byte-identical |
| `crates/nav-xsd-validator/src/validate.rs` | **0** | byte-identical |
| `modules/billing/src/domain/{invoice,mod}.rs`, `api.rs` | **0** | byte-identical |
| `crates/aberp-quote-intake/src/mapping.rs`, `notes_history.rs` | **0** | byte-identical |
| `apps/aberp-ui/ui/src/lib/{issue-invoice,partners,modification}.ts` | **0** | byte-identical |
| `apps/aberp/src/partners.rs` | 11 | trivial |
| `modules/billing/src/adapters/duckdb_store.rs` | 18 | trivial |
| `apps/aberp/src/audit_payloads.rs` | 74 | seam drift, away from hunks |
| `submission_queue.rs` | 109 | ADR-0098 Handle drift, away from hunks |
| `issue_modification.rs` / `issue_storno.rs` | 145 / 147 | ADR-0098 drift, away from hunks |
| `issue_invoice.rs` | 198 | ADR-0098 drift — **overlaps one hunk** |
| `apps/aberp-ui/ui/src/lib/api.ts` | 323 | drift away from hunks |
| **`apps/aberp/src/serve.rs`** | **5264** | Defense's biggest divergence |

Whole-file drift is a red herring for portability — what matters is whether drift overlaps the feature hunks. That was tested directly (§2).

---

## 2. Portability assessment — `git apply --check` verdict per surface

Method: `git -C <portable> diff d2d60ef bb381a2 -- <file>` piped to `git apply --check` against Editions HEAD (strict exact-context match — modifies nothing; stricter than a real 3-way cherry-pick, so CLEAN here ⇒ a cherry-pick is at least as clean).

| Surface | Portable Δ | Verdict | Notes |
|---|---:|---|---|
| `modules/billing/src/domain/vat_rate_kind.rs` | +303 (new file) | **CLEAN (drop-in)** | closed vocab: Percent / AamExempt / DomesticReverseCharge / IntraCommunityGoods=KBAET / IntraCommunityServiceReverse=EUFAD37 + named-deferred markers |
| `modules/billing/src/domain/{invoice,mod}.rs`, `api.rs` | +22 | **CLEAN** | identical base |
| `modules/billing/src/adapters/duckdb_store.rs` | +68 | **CLEAN** | `invoice_line.vat_rate_kind` column + read/write |
| `apps/aberp/src/nav_xml.rs` | +550 | **CLEAN** | `<lineVatRate>` choice + case codes + `summaryByVatRate` mirror — the compliance heart |
| `apps/aberp/src/issue_preflight.rs` | +1045 | **CLEAN** | §4 accept/reject matrix + cross-field matrix |
| `crates/nav-xsd-validator/src/validate.rs` | +151 | **CLEAN** | `vatDomesticReverseCharge` + exemption/out-of-scope case+reason; `communityVatNumber` XOR `customerTaxNumber` |
| `apps/aberp/src/issue_modification.rs` | +97 | **CLEAN** | drift is elsewhere in file |
| `apps/aberp/src/issue_storno.rs` | +97 | **CLEAN** | storno-fold; drift elsewhere |
| `apps/aberp/src/partners.rs` | +114 | **CLEAN** | EU-partner `eu_vat_number` + country-code guard |
| `apps/aberp/src/audit_payloads.rs` | +62 | **CLEAN** | |
| `apps/aberp/src/{print_invoice,submission_queue,notes_history}.rs` | +4 | **CLEAN** | |
| `crates/aberp-quote-intake/src/mapping.rs` | +3 | **CLEAN** | |
| **`apps/aberp/src/serve.rs`** | **+91** | **CLEAN to apply, but MUST be re-implemented** — see §3 | the 130-line patch applies against unrelated context, but its payload (`read_base_line_vat_kinds`) violates a Defense gate as-is |
| **`apps/aberp/src/issue_invoice.rs`** | +143 | **CONFLICT at line 52** — trivial re-anchor | see §3 |
| SPA `IssueInvoice.svelte` / `PartnerForm.svelte` / `ModificationInvoice.svelte` | +242 | **CLEAN** (drift 12/8/4) | per-line VAT-type selector + partner EU-VAT field, no TOML |
| SPA `lib/*.ts` (`api`, `issue-invoice`, `partners`, `modification`) | +408 | **CLEAN** | applies despite api.ts drift 323 |
| all `apps/aberp/tests/*` + `modules/billing/tests/*` | +~1200 | **CLEAN** | incl. `nav_xml_vat_rate_kind.rs` (+310), `nav_xsd_validator_round_trip.rs` (+173) |

**Bottom line: 1 trivial conflict + 1 mandatory re-write. Everything else is a clean patch.** The compliance-critical NAV emit, preflight matrix, and XSD validator all apply byte-clean.

---

## 3. The two things that are NOT a clean cherry-pick

### 3a. `issue_invoice.rs:52` — trivial import re-anchor (5 min)
The failing hunk's context is Portable's import block, which carries `use aberp_db::Handle;` right after the `aberp_billing::{…}` use-list. Editions restructured imports there (ADR-0098), so the context line doesn't match. The *actual* change is one token: add `VatRateKind` to the `aberp_billing::{…}` import. The deeper hunks in the same file (the `CustomerJson.community_vat_number` and `LineJson.vat_rate_kind` `#[serde(default)]` fields) land on stable struct regions and are unaffected. **Verdict: conflicted-but-trivially-adaptable — hand-apply the one-line import, take the rest.**

### 3b. `read_base_line_vat_kinds` in `serve.rs` — mandatory re-write onto the shared Handle (THE Editions deviation)
Portable's new reader — the DB source of truth for the modification silent-downgrade guard (ADR-0101 S2) — opens its **own** connection:
```rust
fn read_base_line_vat_kinds(db_path: &Path, invoice_id: &str) -> Result<Vec<VatRateKind>> {
    let mut conn = Connection::open(db_path) ...   // Portable: registered as opener #91
```
That is legal in Portable (census 90→91). **It is illegal in Defense.** ADR-0098/0099 drove every in-process invoicing/serve opener onto the shared `aberp_db::Handle`; the cut-gate (`tools/cut_gate_db_isolation.sh`) enforces it:
- **CHECK 10f/10g/10h** ban `Connection::open* / Ledger::open / DuckDbBillingStore::open / append_reopen` across the NAV daemons **and the serve.rs request surface** (comment-, string-, and `cfg(test)`-aware; scanner `tools/adr0098_opener_scan.awk`). Only `open_in_memory` / `from_connection` and the one snapshot-EXPORT residual are allow-listed.
- A fresh `Connection::open` in a serve request fn would trip the gate → red on the REQUIRED `cut-gate` check.

The Defense-native shape already exists — the sibling readers this guard sits next to use the shared Handle:
```rust
fn read_invoice_total_gross_minor(db: &aberp_db::HandleArc, invoice_id: &str) -> Result<Option<i64>> {
    let mut conn = db.read().context("shared read: … (ADR-0098 …)")?;   // Defense pattern
```
and `modification_invoice_request` (serve.rs:9975) already holds `state.db` and already calls `read_base_currency(&state.db, …)` / `read_invoice_total_gross_minor(&state.db, …)`.

**Re-write:** port `read_base_line_vat_kinds` to `fn read_base_line_vat_kinds(db: &aberp_db::HandleArc, invoice_id: &str)` using `db.read()`, and change the guard call site from `read_base_line_vat_kinds(&state.db_path, invoice_id)` to `read_base_line_vat_kinds(&state.db, invoice_id)`. The SQL body / `billing::load_ready_invoice_by_id` reuse and the guard logic are unchanged. ~1 function + 1 call site.

**Census consequence — SIMPLER than Portable, but must be verified:** because the Defense reader rides the already-open shared Handle, it adds **zero** new openers. So — unlike Portable's `tools/adr0098_prod_opener_fingerprints.txt` 90→91 registration — **no opener-fingerprint/residual file here should change.** Verify by running the gate locally after the port: `tools/adr0098_r4_opener_fingerprints.txt`, `tools/adr0099_write_fork_residuals.txt`, `tools/adr0098_c2_frozen_residuals.txt` must all stay byte-identical, and CHECK 10f/10g/10h must stay green. **If any opener count moves, the reader is not on the Handle — stop and fix.** (This inverts the Portable census step: Portable *added* a baseline; Defense must prove it *added none*.)

---

## 4. Compliance-critical bits — same adversarial rigor as Portable

These carry real-ÁFA / NAV-filing risk and get the full treatment (Portable cleared two adversarial reviews vs the authoritative NAV XSD before cutting):

1. **NAV case-code emit** (`nav_xml.rs`): the `<lineVatRate>` choice per kind + the correct case/reason codes, and the **`summaryByVatRate` mirror** must agree with the line kinds. This applies clean, but re-verify against the authoritative NAV Online Invoice **v3.0 XSD** (not a hand model) — `vatDomesticReverseCharge`, `vatExemption` (case+reason), `vatOutOfScope` (case+reason).
2. **Cross-field matrix** (`issue_preflight.rs` §4): EU-0 kinds (IntraCommunityGoods/Service) require `customerVatStatus=Other` + a structurally valid `communityVatNumber`; **AAM is buyer-agnostic**; `DomesticReverseCharge` ⇒ `customerVatStatus=Domestic`. Includes the ADR-0102 adversarial fixes: **country-code `[A-Z]{2}` ISO-alpha-2 guard** and AAM buyer-agnosticism.
3. **`customerVatData`** (`nav-xsd-validator`): `communityVatNumber` **XOR** `customerTaxNumber` — never both, never neither for a business buyer.
4. **Modification silent-downgrade guard** (§3b): rejects in-app modification of a non-`Percent` base so an exemption/self-assessment can't silently re-file to NAV as plain 0%.

**Adversarial pass mirrors Portable:** an independent reviewer tries to (a) construct a line/summary kind mismatch the emitter accepts, (b) get an EU-0 line past preflight without a valid `communityVatNumber`, (c) produce a `customerVatData` with both or neither identifier, (d) slip a non-`Percent` base through the modification route. All must be rejected loudly. Port the Portable test corpus verbatim (`nav_xml_vat_rate_kind.rs`, the `nav_xsd_validator_round_trip.rs` additions, the `serve_modification_route.rs` guard tests, `partners.test.ts`) — they apply clean and encode exactly these traps.

---

## 5. Sequenced port sessions (mirrors the proven Portable cadence: design → impl-behind-shut-door → open matrix → adversarial → cut)

Each session ends green on `cargo test` + `cargo clippy -D warnings` + `cargo fmt --check` + the full `tools/cut_gate_db_isolation.sh` (all `ENFORCE_*` on) + `run/dev-test.sh`. Pilot-mode automode may run to **gates-green**; the **cut itself is a held step** (see §7).

- **Session 0 — ADR (design/docs only).** Land Editions ADRs mirroring 0101/0102 (numbering decision in §9). Record the one Editions-specific deviation up front: the `read_base_line_vat_kinds`-on-shared-Handle rewrite + the "zero new openers" census obligation.

- **Session 1 — core machinery behind a shut preflight door.** Port, in one commit (mirrors Portable `33b65e4`): `vat_rate_kind.rs` (drop-in), `modules/billing/domain/{invoice,mod}.rs` + `api.rs` + `duckdb_store.rs` (`invoice_line.vat_rate_kind` column, `#[serde(default)]`→`Percent`), `nav_xml.rs` (+550, clean), `issue_{invoice,modification,storno}.rs` (clean except the `issue_invoice.rs:52` one-line import re-anchor), `quote-intake/mapping.rs`, `nav-xsd-validator/validate.rs`. Feature stays dark: preflight §4 door shut. Then Session-1 tests (`f266537` set) — note that set touches `serve.rs`; take only its test hunks, defer the serve reader to Session 2.

- **Session 2 — open the preflight matrix + the modification guard (the Handle rewrite).** Port `issue_preflight.rs` §4 matrix + storno-fold + `VatRateKind::is_wired` (`15b7a9c`, clean). Port the modification modguard route (`d91d0ee`) **with the §3b rewrite**: `read_base_line_vat_kinds(&state.db: &HandleArc)` via `db.read()`. Port the SPA VAT-kind selector + shared-`LineFormState` compat (`b0fb937`, clean). Run the gate and **prove the opener/residual files are unchanged** (§3b).

- **Session 3 — EU-partner customer type end-to-end (ADR-0102).** Port `partners.rs` (`eu_vat_number` + country-code guard), the cross-field matrix, `customerVatStatus=OTHER` snapshotting onto the invoice, and the SPA `PartnerForm` EU-VAT field + `IssueInvoice` wiring (`1ce5c6e` + `07d8aca` + `18e2314`; all clean). Include the adversarial fixes (country-code, AAM buyer-agnostic).

- **Session 4 — adversarial review vs authoritative NAV XSD.** The §4 red-team, independent reviewer, against the real v3.0 XSD. Two passes, as Portable did.

- **Session 5 — cut prep.** Full gate + `run/dev-test.sh`; confirm backward-compat (§6); confirm census untouched; assemble the `PROD_Defense_v0.x` release branch. **Hold for the human cut.**

---

## 6. Backward compatibility (identical guarantees to Portable, must hold on Defense too)
- `LineJson.vat_rate_kind` and `CustomerJson.community_vat_number` are `#[serde(default)]`. Pre-feature side-stored `input.json` bodies (replayed by storno/modification) and today's SPA/CLI callers deserialize as `Percent` / `None` → the Domestic path → **byte-identical NAV bodies** for every existing invoice. The `nav_xml_notes_never_leak` + round-trip pins guard this; they apply clean.
- The new `invoice_line.vat_rate_kind` DB column must be an additive migration with a default; every existing row reads back `Percent`. Verify the Defense `duckdb_store` migration ordering (Editions has its own migration history) — the column add is clean but confirm it composes with the Defense schema version.

---

## 7. Cut target, release process, pilot posture
- **Version scheme (Defense):** `run/upgrade_defense.sh` enforces `PROD_Defense_v<MAJOR>.<MINOR>[.PATCH]`; `run/run_defense.sh` refuses any HEAD not on an `origin/PROD_Defense_v*` (or legacy `origin/PROD_v*`) tip — a "Frankenstein-build refusal." Current live: **`PROD_Defense_v0.2.11`** (v0.2.10 referenced as shipped in the SVG-lint scripts).
- **The branch IS the release** (`run/release.sh` per ADR-0056: no annotated tag, no local build/codesign; operator clones the branch tip). Shape convention: a **2-segment** bump signals a minor/feature release, **3-segment** a patch. A feature of this size ⇒ **recommend `PROD_Defense_v0.3.0`** (operator's call; `v0.2.12` is the patch-shaped alternative).
- **REQUIRED status check:** the `cut-gate` workflow (`.github/workflows/cut-gate.yml`) — toolchain-free DB-isolation gate (CHECK 1–10, all ENFORCED) + negative-probe harness. Plus `ci.yml` (Rust+Tauri, clippy `-D warnings`, the strict-SVG lint).
- **Pilot posture (per session memory):** Defense is in pilot for ~2 months, no real machines/customers; **automode may run to gates-green, but the cut is a HELD step** (human-initiated). `~/ABERP-Defense` runtime is read-only. Nothing here touches Prod (`~/.aberp/prod`) or the sibling Portable root — the whole port lives under Defense's own tree and DB roots (ADR-0093).

---

## 8. Editions-specific deviations & risks (flagged)
1. **[MUST] `read_base_line_vat_kinds` → shared Handle** (§3b). The single design change forced by divergence. Skipping it = red cut-gate.
2. **[MUST-VERIFY] Census inverts:** prove **zero** opener/residual churn, rather than registering a new opener as Portable did (§3b). If a count moves, the reader isn't on the Handle.
3. **[TRIVIAL] `issue_invoice.rs:52`** import re-anchor (§3a).
4. **[VERIFY] DB migration composition** — the `invoice_line.vat_rate_kind` column add must slot into Defense's own migration history (§6).
5. **No new invoice surfaces in Defense** that would need extra VAT wiring: the NAV emit/preflight/validator/customer model are the *same* files as Portable (all at the shared prod base), and Defense's extra strands (digital-ID, firing-sites, heat-lot, MES/QA) are orthogonal to invoicing. `restore_from_nav_extract.rs` already parses `customerVatStatus` DOMESTIC/PRIVATE_PERSON/**OTHER** — the OTHER path the EU-partner feature needs is already recognized on the restore side.
6. **serve.rs merge caution:** although the 130-line VAT patch applies clean today, serve.rs is Defense's most volatile file (drift 5264, actively churned by ADR-0098/0099). Port serve.rs hunks **last within each session** and re-run `git apply --check` against the then-current HEAD before hand-applying, in case intervening Defense work shifts the context.

## 9. Open decision — ADR numbering
Defense's ADR log is independent and currently ends at **0099** (0100 is free). Two options:
- **(recommended) Mirror the Portable numbers** — Editions `ADR-0101` (VAT rate-kind port) + `ADR-0102` (EU-partner port), each headed "Defense port of ABERP.git ADR-0101/0102" — so cross-repo provenance is obvious and the compliance story is traceable by number. Leaves Editions 0100 as a deliberate gap (document it).
- **(alternative) Contiguous** — Editions `ADR-0100` + `ADR-0101`, no gap, but the numbers no longer line up with Portable.

This is the one choice needing Ervin's call before Session 0.

---

### Appendix — Portable feature commit map (for cherry-pick reference)
`d2d60ef..bb381a2` on `ABERP.git`:
- `16ffbcb` ADR-0101 design · `33b65e4` core machinery · `f266537` S1 tests · `15b7a9c` preflight §4 + selector · `b0fb937` SPA selector tests · `b9faa43` fmt · `d91d0ee` modguard route (serve.rs — **rewrite target**) · `a042485` ADR-0102 design · `1ce5c6e` EU-partner e2e · `07d8aca` ADR-0102 tests · `18e2314` adversarial fixes (country-code + AAM) · `35bb96f` census baseline (**Defense equivalent = prove zero churn**) · `bb381a2` cut.
