# ADR-0102 (Defense) — EU-partner customer type: intra-Community 0% VAT invoices end-to-end (port of ABERP.git ADR-0102)

- **Status:** **Proposed on Defense — DESIGN PORTED (Session 1, 2026-07-16); IMPLEMENTATION DEFERRED to a later Defense port session.** This is the Defense-line port of Portable **ABERP.git ADR-0102**. Unlike Portable (where 0102 was implemented in-session), on Defense **only the design/ADR is landed now**; the EU-partner code (partner `eu_vat_number` validation, `customerVatStatus=OTHER` emit, cross-field matrix, SPA) lands in a later session (port plan §5, "Session 3 — EU-partner end-to-end"), after ADR-0101's preflight matrix is opened (Defense "Session 2").
- **Date:** 2026-07-16
- **Deciders:** Ervin Áben (greenlit the VAT feature). Port design by Dispatch.
- **Port provenance:** Portable `ABERP.git` ADR-0102 (design + amendment `9a` NAV-adversarial fixes), shipped in `PROD_v2.32.0` @ `bb381a2`. See `docs/PORT-PLAN-vat-rate-kind-eu-partner.md`.
- **Companion:** Defense **ADR-0101** (per-line `vat_rate_kind`) — same mirror-numbering decision (ADR-0101 §D.1); this closes the ADR-0101 gap where the two intra-Community line kinds (`IntraCommunityGoods`=KBAET / `IntraCommunityServiceReverse`=EUFAD37) are emittable at the line but un-assemblable without a buyer carrying `customerVatStatus=OTHER` + a `communityVatNumber`.
- **Governance note:** as for ADR-0101, "CLAUDE.md rule N" references are Portable conventions; the Defense analogues are `SAW-OFF.md` / `FOUNDATION.md` / `adr/` + the cut-gate.

---

## D. Defense port decisions & deviations (read first)

### D.1 ADR numbering — mirror, not contiguous (⚑ FLAG — see ADR-0101 §D.1)
Mirror-numbered as Defense **ADR-0102** to line up with Portable; the deliberate Defense **ADR-0100 gap** is documented in ADR-0101 §D.1. ⚑ Ervin may override to contiguous.

### D.2 Implementation deferred — design only in Session 1
The Session-1 Defense port kept the **preflight door shut** (ADR-0101 §D.5): no non-`Percent` line kind can be issued yet, so the EU-partner buyer machinery this ADR describes has **no live surface to attach to** yet. Landing the ADR now preserves cross-repo traceability and records the design; the code (per-layer map §5) is a later Defense session. When it lands it MUST include Portable's amendment `9a` NAV-adversarial fixes — the ISO-alpha-2 `[A-Z]{2}` country-code guard (FIX #1) and **AAM buyer-agnostic** (FIX #2) — which are already folded into §4/§9a below.

### D.3 Reuse posture unchanged on Defense
The `partners.eu_vat_number` column already exists on Defense (same shared prod base as Portable), so §3.1's "reuse, don't add a column" reuse decision holds identically. The snapshot triple (§3.2) and the buyer-status↔line-kind cross-field matrix (§4) port unchanged. No Defense-specific code deviation is anticipated for 0102 (contrast ADR-0101 §D.2, which was serve.rs-opener-specific); this will be re-verified at implementation time, and any serve.rs touch re-checked against the then-current cut-gate.

### D.4 NAV-category adversarial before the Defense cut
Portable cleared **two** adversarial reviews vs the authoritative NAV OSA 3.0 XSD before cutting; the Defense port repeats that pass (port plan §5, "Session 4 — adversarial review vs authoritative NAV XSD") before the held `PROD_Defense_v0.3.0` cut.

---

## Inherited Portable design (verbatim from ABERP.git ADR-0102 @ bb381a2)

> Authoritative feature design carried verbatim for the cross-repo compliance record. File:line references are Portable's (compliance-core files are byte-identical on Defense). Portable's §9 "stacks on `vat-rate-kind-s2-modguard`, cuts together, port to Defense is a later effort" is exactly the effort this Defense ADR belongs to. Amendment §9a's fixes are REQUIRED at Defense implementation time (D.2).


- **Status:** **Proposed → Implemented (this session, on branch `vat-rate-kind-s3-eupartner` off `vat-rate-kind-s2-modguard` @ d91d0ee).** Companion to **ADR-0101** (per-line `vat_rate_kind`). Where ADR-0101 wired the *line-level* NAV 0%/exempt sub-types, this ADR wires the *buyer-level* representation those sub-types require, closing the NAV-adversarial's **finding #2**.
- **Date:** 2026-07-16
- **Deciders:** Ervin Áben (greenlit the VAT feature "as many sessions as we need"). Design + implementation by Dispatch this session.
- **Closes:** ADR-0048 §7's named-deferral of the `Other` (foreign buyer) `customerVatStatus`, **narrowed to the EU-community-VAT sub-shape only** (non-EU `thirdStateTaxId` stays named-deferred — see §8). Closes the ADR-0101 gap where `IntraCommunityGoods` (KBAET) / `IntraCommunityServiceReverse` (EUFAD37) were *emittable at the line* but *un-assemblable* because no buyer representation carried `customerVatStatus=OTHER` + a `communityVatNumber`.
- **Related:** ADR-0101 (per-line `vat_rate_kind`), ADR-0048 (customer VAT status closed vocab — this ADR implements its §7 v2), ADR-0042 (notes NEVER on NAV wire — PII firewall this ADR must not breach), ADR-0022 (`nav-xsd-validator`), ADR-0049 (storno/modification replays side-stored `input.json`). NAV wire-shape gotchas: `reference-nav-gotchas` memory §1 (the `customerVatData`/status matrix this ADR extends).

---

## 0. TL;DR

ADR-0101 landed the machinery to emit `<vatExemption case=KBAET>` and `<vatOutOfScope case=EUFAD37>` per line. But NAV's business rules bind those categories to the **buyer**: an intra-Community 0% invoice is only well-formed when the buyer is a **foreign-EU business** — NAV `customerVatStatus=OTHER` carrying `<customerVatData><communityVatNumber>…</communityVatNumber></customerVatData>`. ABERP's `Other` status was named-deferred (ADR-0048 §7): a domestic buyer needs a Hungarian ADÓSZÁM (won't fit an EU partner), `PrivatePerson` forbids `customerVatData`, and `Other` was rejected at preflight + emit. So a *correct* EU-0 invoice was **un-assemblable** — the only way to issue one was with a semantically-wrong domestic buyer (a compliance error the loose local validator would not catch).

This ADR:
1. **Un-defers `Other`** end-to-end for the **EU-community-VAT** sub-shape (partner → preflight → emit → validator → audit → SPA).
2. **Reuses the existing dormant `partners.eu_vat_number` column** as the community-VAT source (no new column — CLAUDE.md rules 2/8/12), **snapshots** it onto the issued invoice's immutable wire body + on-disk NAV XML + audit payload (mirroring how buyer name/address/tax_number already snapshot), so later partner edits never rewrite an issued invoice.
3. Adds a **structural VIES-shape validator** for the EU VAT number (country-prefix + format). A **live VIES online check is OUT OF SCOPE** (§8, flagged).
4. Enforces the **cross-field matrix** (§4) at preflight: the two EU-0 kinds REQUIRE `Other` + a valid `communityVatNumber`; `AamExempt`/`DomesticReverseCharge` REQUIRE `Domestic`; `Percent` is buyer-agnostic. Rejecting loudly closes the "assemblable only with a semantically-wrong domestic buyer" risk.
5. **Backward-compat is byte-identical:** `customerVatStatus` defaults to `Domestic`; only `Other` buyers hit any new code path.

---

## 1. Current state (grep-verified on branch d91d0ee)

| Layer | Today | File:line |
|---|---|---|
| `CustomerVatStatus` enum | Domestic / PrivatePerson / **Other**. `Other` fully modeled (`as_nav_token`→`"OTHER"`, `as_db_str`, `from_db_str`) but **loud-fails** at preflight + emit | `apps/aberp/src/nav_xml.rs:65` |
| `write_customer` | Domestic emits structured `customerTaxNumber`; PrivatePerson emits nothing; **`Other` → `anyhow!` "v1 named-deferred"** | `apps/aberp/src/nav_xml.rs:1205-1210` |
| Preflight customer block | Domestic/PrivatePerson wired; **`Other` → `CustomerVatStatusOtherNotSupportedV1`** | `apps/aberp/src/issue_preflight.rs:735-737` |
| Validator `walk_customer_vat_data` | accepts **only** `customerTaxNumber` (structured, `common:` prefix) | `crates/nav-xsd-validator/src/validate.rs:535-539` |
| Partner model | already carries `eu_vat_number: Option<String>` (dormant: stored + shown in PartnerForm/List, **never validated, never on NAV wire**) | `apps/aberp/src/partners.rs:226,281` |
| Wire body `CustomerJson` | carries `vat_status`, `tax_number`, `name`, `address` — **no community VAT number** | `apps/aberp/src/issue_invoice.rs:233-274` |
| Line VAT-kind matrix (ADR-0101 S2) | per-line kind accept/reject + mixed-kind guard live; **no cross-field buyer-status check** | `apps/aberp/src/issue_preflight.rs:760-819` |
| SPA IssueInvoice | radio has an `Other` option **disabled** ("v2-ben jön"); address fields wrongly optional for Other | `apps/aberp-ui/ui/src/routes/IssueInvoice.svelte:992-1004,1047-1078` |
| SPA PartnerForm | radio `Other` **disabled**; `euVatNumber` a free-text optional field | `apps/aberp-ui/ui/src/routes/PartnerForm.svelte:189-200,267-278` |

**Key reuse decision:** `partners.eu_vat_number` already exists and is surfaced in the SPA. Adding a *second* `community_vat_number` column (as ADR-0048 §7 sketched) would duplicate a field the codebase already has (CLAUDE.md rule 8 — "add a duplicate function next to an identical one it never read"). **This ADR reuses `eu_vat_number` as the community-VAT source.** The NAV-facing wire element is `communityVatNumber`; the partner business-attribute column stays `eu_vat_number`; the SPA composer maps one to the other.

---

## 2. NAV mapping — the `Other`/`customerVatData` shape (grep-verified against the existing emit + ADR-0048)

ADR-0048's Context table (grep-verified, and NAV-confirmed via the invoice-18 `CUSTOMER_DATA_EXPECTED` forensic it cites) already pins the three-way matrix:

| `customerVatStatus` | `customerVatData` | `customerName` / `customerAddress` |
|---|---|---|
| `DOMESTIC` | REQUIRED — structured `<customerTaxNumber>` (`common:taxpayerId`/`vatCode`/`countyCode`) | REQUIRED |
| `PRIVATE_PERSON` | FORBIDDEN | FORBIDDEN on wire (§169 governs PDF only) |
| **`OTHER`** | **REQUIRED** — inner choice `<communityVatNumber>` (EU) **XOR** `<thirdStateTaxId>` (non-EU) | **REQUIRED** (`CUSTOMER_DATA_EXPECTED` predicate is `status != PRIVATE_PERSON`) |

The exact emit for the EU sub-shape (this session wires **only** the `communityVatNumber` arm):

```xml
<customerInfo>
  <customerVatStatus>OTHER</customerVatStatus>
  <customerVatData>
    <communityVatNumber>ATU12345678</communityVatNumber>
  </customerVatData>
  <customerName>…</customerName>
  <customerAddress><common:simpleAddress>…</common:simpleAddress></customerAddress>
</customerInfo>
```

- `communityVatNumber` is a **flat text** element in the OSA **data** namespace (like `customerName`/`customerVatStatus`) — **no `common:` prefix** (contrast the structured `customerTaxNumber`, whose children ARE `common:`-prefixed). Emitted via the prefix-less `text_element` helper.
- `customerName` + `customerAddress` for `Other` are already handled by `write_customer`'s existing `if !matches!(PrivatePerson)` branch (Other is non-PrivatePerson → both emitted). No change there.

**Provenance caveat (flagged):** as ADR-0101/§2 documents, the repo vendors no NAV XSD; the `communityVatNumber` element name + its data-namespace placement are best-knowledge NAV OSA 3.0 (`CustomerVatDataType` choice). This is the primary item for the **NAV-category adversarial** before the cut (§9).

---

## 3. Data model + snapshot

### 3.1 Partner (master data) — reuse `eu_vat_number`

No schema change. `partners.eu_vat_number: Option<String>` **is** the community VAT number. It becomes **required + structurally-validated** at the partner-form gate **only** when `customer_vat_status == Other` (Domestic/PrivatePerson keep it optional free-text — existing rows unaffected).

### 3.2 Per-invoice snapshot (the "survives a later partner edit" invariant)

The community VAT number is snapshotted onto the issued invoice exactly like the buyer's name/address/tax_number already are — it rides the **immutable** triple:
1. **wire body** `CustomerJson.community_vat_number` (side-stored `input.json`, replayed verbatim by storno/modification per ADR-0049);
2. **on-disk NAV XML** (`<communityVatNumber>` inside the emitted `customerVatData` — the canonical record of what NAV saw, never rewritten, per `reference-nav-gotchas §3`);
3. **audit payload** `InvoiceDraftCreatedPayload.customer_community_vat_number` (tamper-evident, mirrors the existing `customer_vat_status` field + `with_customer_vat_status` builder).

The SPA populates the wire field from the picked partner's `eu_vat_number` at *issue time*; a later edit to `partners.eu_vat_number` does not touch any of the three. **This is the same denormalise-at-issuance snapshot pattern PR-77 used for the address quartet and PR-73 used for the seller bank account** — not a new mechanism.

### 3.3 EU VAT number structural validator (VIES shape)

`validate_community_vat_number(&str) -> Result<(), String>`, applied at both the partner-form gate and invoice preflight:
- normalise: uppercase, strip spaces;
- 2-letter prefix ∈ the EU-VAT country set (incl. `EL` for Greece, `XI` for Northern Ireland; `HU` included for structural completeness);
- remainder: 2–12 alphanumeric chars (`[0-9A-Z]`);
- **no checksum, no per-country length table, no live VIES lookup** (§8 — flagged out of scope).

---

## 4. THE CROSS-FIELD MATRIX (the compliance crux) — enforced at preflight

Two orthogonal rules, both invoice-scoped (customer status is invoice-level):

**(a) Buyer-status ↔ line-kind matrix.** For the invoice's operative non-`Percent` kind (ADR-0101's mixed-kind guard already forces ≤1 distinct non-Percent kind per invoice):

| line `vat_rate_kind` | required `customerVatStatus` | rationale |
|---|---|---|
| `Percent` | **any** (Domestic / PrivatePerson / Other) | numeric rate is buyer-agnostic (ADR-0101 hold) |
| `AamExempt` (AAM) | **Domestic** | ⚠ see §10.2 — modeled strict per task spec |
| `DomesticReverseCharge` | **Domestic** | "belföldi" = domestic buyer by definition (Áfa tv. §142) |
| `IntraCommunityGoods` (KBAET) | **Other** | intra-Community supply is *to a foreign-EU business* (§89) |
| `IntraCommunityServiceReverse` (EUFAD37) | **Other** | place of supply is the buyer's member state (§37) |

Reject loud: `VatKindRequiresOtherBuyer` / `VatKindRequiresDomesticBuyer`.

**(b) `Other` buyer completeness** (NAV `customerVatData` REQUIRED for OTHER, independent of line kind): an `Other` buyer REQUIRES a present + structurally-valid `communityVatNumber` and MUST NOT carry a HU ADÓSZÁM. Reject loud: `CommunityVatNumberMissing` / `CommunityVatNumberMalformed` / `CustomerTaxNumberPresentForOther`.

The two combine so the dangerous state — *EU-0 line assembled against a domestic buyer* — is rejected, and the only accept path for an EU-0 invoice is `Other` + valid EU VAT number. The **safe** direction of error (over-strict → blocks a possibly-valid invoice) is preferred over the **dangerous** one (lenient → wrong ÁFA to NAV).

---

## 5. Per-layer change map

| Layer | Change | File |
|---|---|---|
| `CustomerInfo` | add `community_vat_number: Option<String>` | `apps/aberp/src/nav_xml.rs:328` |
| `write_customer` | `Other` branch: emit `<customerVatData><communityVatNumber>` (loud-fail if `None`) | `nav_xml.rs:1205` |
| Validator | `walk_customer_vat_data` accepts `customerTaxNumber` **XOR** `communityVatNumber` (choice, exactly-one; flat text for the latter) | `validate.rs:535` |
| Wire body | `CustomerJson.community_vat_number: Option<String>` (`#[serde(default)]`, rename `communityVatNumber`) | `issue_invoice.rs:233` |
| Hydration | thread `community_vat_number` into `CustomerInfo` | `issue_invoice.rs:819` |
| Preflight | `Other` customer branch (require valid community VAT + forbid tax_number + require address) + cross-field matrix (§4) + new error variants | `issue_preflight.rs` |
| Community-VAT validator | `validate_community_vat_number` (shared preflight + partner form) | new fn in `nav_xml.rs` (next to `parse_hungarian_tax_number`) |
| Partner validation | `Other` branch requires + validates `eu_vat_number` | `partners.rs:552` |
| Audit payload | `customer_community_vat_number: Option<String>` + `with_customer_community_vat_number` builder | `audit_payloads.rs:251` |
| SPA api.ts | `communityVatNumber?` on customer wire body | `api.ts:737` |
| SPA issue-invoice.ts | form field `customerCommunityVatNumber`; composer emits it for Other; `vatKindBuyerMismatch` helper | `issue-invoice.ts` |
| SPA partners.ts | `buyerFieldsFromPartner` pulls `eu_vat_number` + passes through non-HU country for Other | `partners.ts:227` |
| SPA IssueInvoice.svelte | enable `Other` radio; community-VAT input (shown+required for Other); fix address-required for Other; inline cross-field guidance | `IssueInvoice.svelte` |
| SPA PartnerForm.svelte | enable `Other` radio; make `euVatNumber` required+validated for Other | `PartnerForm.svelte` |

---

## 6. Backward compatibility (HARD invariant)

1. `customerVatStatus` defaults to `Domestic` at every layer (serde `#[serde(default)]`, DB `DEFAULT 'Domestic'` from ADR-0048). A pre-ADR-0102 body/partner is unchanged.
2. `community_vat_number` is `Option` + `#[serde(default)]` everywhere → absent in every existing wire body / input.json / audit payload → deserialises to `None` → Domestic path, no emit change.
3. `write_customer` is branch-guarded: Domestic + PrivatePerson code paths are **untouched**; only `Other` reaches new emit code. Every existing on-disk NAV XML is byte-identical.
4. Storno/modification of a pre-0102 base replays `Domestic` + `None` → reproduces original bytes.
5. **Regression pin:** the existing domestic golden emit test must stay byte-identical (impossible to pass if the Domestic path changed — CLAUDE.md rule 9).

---

## 7. Test plan

- **NAV round-trip (EU-0):** an `Other` buyer + `IntraCommunityGoods` line emits `customerVatStatus=OTHER` + `<customerVatData><communityVatNumber>` + the KBAET line, feeds through `nav-xsd-validator` → passes; same for `IntraCommunityServiceReverse`/EUFAD37.
- **Cross-field matrix:** EU-0 + Domestic → reject `VatKindRequiresOtherBuyer`; EU-0 + Other + valid EU VAT → accept; AAM + Other → reject `VatKindRequiresDomesticBuyer`; DRC + Other → reject; Percent + Other → accept; Other + missing/malformed community VAT → reject.
- **EU VAT structural validation:** `ATU12345678` ok; `HU12345678` ok; `ZZ123` reject (bad prefix); `AT` reject (empty body); `AT!!!` reject (non-alnum).
- **Backward-compat golden:** a Domestic/`Percent` invoice emits byte-identically to the pre-0102 snapshot; a pre-0102 input.json (no community field, no vat_status) round-trips through storno unchanged.
- **PII/notes firewall (ADR-0042):** per-line note still never on the wire; `communityVatNumber` is a buyer VAT id (structural, not free-text PII beyond what NAV requires).
- **Partner snapshot survives edit:** issue an Other invoice; edit `partners.eu_vat_number`; the issued invoice's snapshot (input.json / NAV XML / audit payload) is unchanged.
- **Validator:** `communityVatNumber` accepted under `customerVatData`; both-children (`customerTaxNumber` + `communityVatNumber`) → CardinalityExceeded; `customerVatData` empty → MissingRequiredChild.

---

## 8. Out of scope (flagged)

1. **Non-EU `thirdStateTaxId` arm** of `customerVatData` — stays named-deferred (this session's gap is the *EU* 0% kinds, which need `communityVatNumber`). The validator loud-fails `thirdStateTaxId` as `UnexpectedElement` (ABERP never emits it) — conservative.
2. **Live VIES online validation** of the EU VAT number — a network call to the EU VIES SOAP service. Only the **structural** shape is checked here. Flagged for a future ADR.
3. **Closed-vocab country** for the buyer address — `buyerFieldsFromPartner` currently forces `HU`; this ADR passes the partner's country through for `Other` (best-effort, editable on the form) but does not add a country vocabulary/validator. Flagged.

---

## 9. Sequencing note

This ADR is the **buyer-side** half of the ADR-0101 VAT feature; it stacks on `vat-rate-kind-s2-modguard` so the whole feature (line kinds + EU buyer) cuts together. **Next (separate effort): a NAV-category adversarial on the complete EU-0 path** — confirm the `communityVatNumber` element name/namespace and the §10.2 AAM-strictness against the published NAV OSA 3.0 code list — **then the cut.** Porting the feature to the Defense line (ABERP-Editions.git) is a later, separate effort.

## 9a. Amendment 2026-07-16 — NAV-category adversarial fixes (branch `vat-rate-kind-s4-eu0fixes` off `07d8aca`)

The NAV-category adversarial (§9) CONFIRMED the `customerVatData` / `communityVatNumber` element-and-namespace mapping correct against the authoritative NAV OSA 3.0 XSD (`CustomerVatDataType` is an `xs:choice`; `communityVatNumber` is a flat, data-ns element — the repo's shape is exactly right). It surfaced two loud-fail defects (neither a silent wrong-ÁFA), both fixed here:

- **FIX #1 (must — before the first real EU-0 submit): structural ISO-alpha-2 guard on the `Other` buyer's `country_code`.** NAV's `CountryCodeType` is `[A-Z]{2}`; an `Other` buyer's country flows from the free-form partner record, so a partner saved as "Austria"/"Ausztria" would emit `<countryCode>AUSTRIA</>` and bounce at submit (burning a sequence). Added `nav_xml::validate_country_code` (verbatim-strict `[A-Z]{2}`) + a new preflight variant `CustomerCountryCodeInvalid` in the `Other` branch (symmetric with `validate_community_vat_number`), turning a NAV bounce into an operator-correctable preflight error. Deleted the DEAD `/^[A-Za-z]{2}$/` branch in SPA `partners.ts::foreignCountryToCode` (both arms returned `trimmed.toUpperCase()`). Scoped to `Other` only — Domestic buyers are HU-forced by the SPA (backward-compat). Full closed-vocab country list stays deferred (§8.3); structural `[A-Z]{2}` is enough.
- **FIX #2 (correctness): `AamExempt` is now BUYER-AGNOSTIC.** AAM (alanyi adómentesség §187) is a SELLER-side exemption — NAV binds no `customerVatStatus` to it — so the §4(a) "AAM requires Domestic" rule (flagged strict in §10.2 below) wrongly blocked the common legitimate case (exempt small business → magánszemély, or → EU buyer). AAM now accepts any buyer status, like `Percent`. **`DomesticReverseCharge` stays Domestic-only** (§142 belföldi = two domestic taxable persons — genuinely buyer-constrained). The §4(a) matrix table row for AAM should read **any** (like `Percent`); only DRC + the two intra-Community kinds remain buyer-constrained.

Also added (adversarial SHOULD-ADD, test coverage): an EUFAD37 (intra-Community SERVICE reverse-charge) customer+line combined round-trip (`vatOutOfScope`/EUFAD37), sibling to the existing KBAET round-trip. A full serve-route EU-0 end-to-end issuance test remains a flagged follow-up (the render→validate combined round-trip + the preflight unit suite cover emit and gate; the serve harness (DB Handle + router) is heavier than this pre-cut fix warrants).

Backward-compat unchanged for Domestic / PrivatePerson / Percent and every existing invoice.

## 10. Flagged assumptions (conservative; no AskUserQuestion per session constraint)

1. **`communityVatNumber` element name + data-namespace placement** are best-knowledge NAV OSA 3.0, not repo-verified (no vendored XSD). Primary adversarial item.
2. **AAM-requires-Domestic** is modeled strict per the task's explicit matrix. AAM (alanyi adómentesség) is a *seller-side* exemption; whether NAV strictly forbids an `Other` buyer on an AAM line is softer than the DomesticReverseCharge case ("belföldi" is unambiguously domestic). Modeled strict = the safe direction (blocks rather than mis-files); flagged for adversarial confirmation.
3. **`eu_vat_number` reused as the community-VAT source** (not a new `community_vat_number` column) — the codebase already has the column + SPA field. Domestic/PrivatePerson partners keep it as optional free-text metadata; only `Other` requires+validates+snapshots it.
4. **`Other` buyer forbids a HU tax number** (mirrors PrivatePerson's symmetric rule) — modeled as a loud preflight/partner-form reject rather than silently ignored, for hülye-biztos consistency.
5. **Buyer address country for `Other`** passed through from the partner (best-effort) rather than forced `HU`; full country vocab deferred (§8.3).
