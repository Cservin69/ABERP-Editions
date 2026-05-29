# ADR-0048 — Customer VAT Status: Closed Vocab for Natural-Person + Domestic + Foreign Buyers

**Status:** Accepted — PR-97 (2026-05-28). v1 (Domestic + PrivatePerson) shipped; Other named-deferred to v2 per §7.

## Amendments at PR-97 build time (Ervin overrides to the ADR-as-proposed)

These changes are load-bearing — applied during the PR-97 build and
landed atomically with the rest of the v1 ship. They override the
corresponding §"Open questions" answers in the original ADR text.

### Override 1 — Field-selective lock (NOT whole-record + NOT re-selectable)

Open-question #1 of the original ADR proposed an "interactable
radio with audit" posture. Ervin's final call: a partner's two
**intrinsic-identity fields** (`tax_number` AND
`customer_vat_status`) become **READ-ONLY** the moment the partner
has ≥1 issued invoice referencing it. Other fields (address, email,
display_name, legal_name) STAY editable — companies rename, move
addresses, change emails; only the legal identity is locked.

> Ervin verbatim: "if it changes, it is a new partner."

Detection: persistent counter (`partners.issued_invoice_count BIGINT
NOT NULL DEFAULT 0`) incremented in the SAME tx as the audit-ledger
write at issue time. Wire body's `customer.partnerId` carries the
SPA-selected partner id; the issue path runs `UPDATE partners SET
issued_invoice_count = issued_invoice_count + 1 WHERE id = ?` inside
the tx. Pre-PR-97 historical invoices are detected via a fallback
audit-ledger scan (`partner_has_issued_invoices` in `serve.rs`) so
existing partners are correctly locked after the migration.

UX: PartnerForm shows the bilingual hint "Zárolva, mert már történt
számlázás — módosításhoz hozz létre új partnert. / Locked because
invoices have been issued — to change, create a new partner." next
to the two locked fields. The Külföldi radio option stays disabled
with its v2-deferral hint regardless of lock state.

Backend defence in depth: `update_partner` in `partners.rs`
preserves the existing `tax_number` and `customer_vat_status` values
verbatim when `has_issued_invoices` is true, even if the wire body
asks for different values. A curl bypass cannot mutate locked
fields.

### Override 2 — PrivatePerson GDPR posture: BOTH name AND address optional

Open-question #5 of the original ADR proposed "keep PDF address
optional; surface as a 'fill in for the PDF' hint." Ervin's final
call: under PRIVATE_PERSON, BOTH `customerName` AND
`customerAddress` are operator-OPTIONAL — the GDPR posture allows
ABERP to record zero identifying detail for a natural-person buyer
beyond the closed-vocab status.

NAV XSD verification status: NAV v3.0 `CustomerInfoType` declares
`<customerName>` as `minOccurs="0"` at the schema level. The
PRIVATE_PERSON + omitted-name combination has not been exercised
against the live NAV-test endpoint as of PR-97 — first such
issuance will confirm the business-rule layer also permits the
omission. If NAV rejects, revert the validator's
`customerVatStatus`-only ORDERED_REQUIRED set + the emitter's
omit-when-empty branch and reinstate the unconditional
`customerName` required pin.

Wire body: empty-string for `customerName` or `address` under
PRIVATE_PERSON causes the emitter to OMIT the elements from the NAV
XML. Validator: relaxed `ORDERED_REQUIRED` for `customerInfo` to
only `customerVatStatus` (was `["customerVatStatus", "customerName"]`).
Preflight: `CustomerNameEmpty` now CONDITIONAL on `vat_status !=
PrivatePerson`. PDF: skips the buyer-name slot AND the ADÓSZÁM label
when the corresponding field is empty.

### v2 Külföldi sub-shape — dropdown (NOT sub-radio)

Open-question #4 of the original ADR proposed a sub-radio for the EU
community-VAT vs non-EU third-state-tax-id distinction. Ervin's
preference for v2: a **DROPDOWN** to save vertical space.

> v2 PR will surface the choice as a dropdown
> (`EU community VAT | Non-EU third-state tax-id`) with a single
> text input below that morphs its placeholder + validation rule
> based on the dropdown's selected value. No v1 surface change.

### Meta-principle (no SQL-engine-specific constructs)

Ervin verbatim: "SQL is current choice not baked in — I do not want
anything SQL-specific."

Implication for this ADR + future ones:

- NO `CHECK (column IN (...))` constraints on closed-vocab columns.
  Validate at the application layer (`validate_partner_inputs` for
  `customer_vat_status`).
- NO triggers, NO stored procedures, NO views, NO foreign-key
  cascades that encode business rules.
- The pre-existing `partners.kind CHECK (kind IN
  ('Customer','Supplier','Both'))` from PR-48α is grandfathered (not
  PR-97 to relitigate); new PR-97 columns (`customer_vat_status`,
  `issued_invoice_count`) carry NO CHECK constraints.
- Migration SQL stays portable: `ADD COLUMN IF NOT EXISTS` + `UPDATE
  ... WHERE col IS NULL` (DuckDB rejects `ADD COLUMN ... NOT NULL
  DEFAULT 'X'` anyway, so portable shape is also the only working
  shape).


**Author:** Ervin Áben (ABERP), session brief on magánszemély (natural-person) buyers.
**Supersedes / amends:** none (additive surface).
**Related:**
- [ADR-0009 (NAV invoice issuing)](0009-invoice-state-machine-and-audit-trail.md)
- [ADR-0022 (NAV XSD runtime validator)](0022-nav-xsd-runtime-validator.md) — and its PR-77 addendum on
  business-rule-vs-schema-rule trap doors.
- [ADR-0037 (EUR invoicing — closed-vocab Currency)](0037-eur-invoicing-compliance.md) — the closed-vocab posture this ADR mirrors.
- [ADR-0038 (Invoice preflight validation)](0038-invoice-preflight-validation.md) — the surface the new conditional rule lives on.
- [ADR-0041 (ERP module architecture)](0041-erp-module-architecture.md) — Partners is master-data, products is the precedent (ADR-0046).
- [ADR-0042 (Notes never on NAV wire)](0042-invoice-notes-never-in-nav-xml.md) — load-bearing-invariant precedent.
- [ADR-0046 (Product unit of measure — NAV-aligned closed vocab)](0046-product-unit-of-measure-nav-aligned.md) — closed-vocab-with-escape-hatch precedent for the foreign-buyer branch.
- **PR-77** (`_handoffs/PR-77-handoff.md`) — landed the DOMESTIC branch (`<customerVatStatus>DOMESTIC</>` + `<customerVatData>` + `<customerAddress>`); this ADR closes its open PRIVATE_PERSON branch.

## Context

Pre-PR-96 ABERP unconditionally emits `<customerVatStatus>DOMESTIC</customerVatStatus>` plus a structured `<customerVatData><customerTaxNumber>…</></>` plus `<customerAddress>` (PR-77 hold). The Hungarian ADÓSZÁM field is treated as **universally mandatory** in three places:

1. **Partner storage** — `apps/aberp/src/partners.rs:155,332` — `tax_number VARCHAR NOT NULL`.
2. **Partner form validation** — `apps/aberp/src/partners.rs:308-313` — `validate_tax_number` fires `tax number is required` on blank.
3. **Invoice preflight** — `apps/aberp/src/issue_preflight.rs:86,444` — `InvoicePreflightError::CustomerTaxNumberMissing` fires when `customer.tax_number.trim().is_empty()`.

Hungarian invoice law differentiates buyers along NAV's `customerVatStatusType` enum (NAV `Online Számla` v3.0, schema element `customerInfo/customerVatStatus`):

| Token            | Meaning                                                                  | `customerVatData` requirement                                                                  |
| ---------------- | ------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `DOMESTIC`       | Hungarian taxable entity (the case ABERP currently ships).               | **REQUIRED** — must carry `<customerTaxNumber>` with structured taxpayerId/vatCode/countyCode. |
| `PRIVATE_PERSON` | Hungarian or foreign natural person (magánszemély).                      | **FORBIDDEN** — element MUST NOT appear (NAV rejects PRIVATE_PERSON + customerVatData as a body-shape violation).  |
| `OTHER`          | Non-Hungarian taxable entity (EU community VAT, or non-EU third-state).  | **REQUIRED** with a different inner choice: `<communityVatNumber>` (EU) **XOR** `<thirdStateTaxId>` (non-EU). |

Ervin's UX proposal: a buyer-type radio (Adóalany / Magánszemély / Külföldi). When the operator selects **Magánszemély**, the ADÓSZÁM input is disabled (not removed) and not required. When the operator selects **Külföldi**, the input morphs (EU community-VAT vs non-EU third-state-tax-id).

Today the PRIVATE_PERSON case **cannot be issued** through ABERP at all — the preflight rejects `CustomerTaxNumberMissing` before the pipeline ever reaches the NAV emitter. PR-77's strengthened validator (`crates/nav-xsd-validator/src/validate.rs:403-488`) correctly handles the PRIVATE_PERSON branch (does NOT require customerVatData when status equals PRIVATE_PERSON, see line 466-481) but the upstream surfaces never feed it that status. The validator's PRIVATE_PERSON branch is therefore **untested in practice** — only the negation pin `private_person_status_does_not_require_customer_vat_data_or_address` exists (PR-77 handoff §Forensic Step 3).

This ADR closes the gap end-to-end: model the closed vocab, plumb it through partners → form → wire → emitter → validator → preflight → audit-payload, and define the v1 scope (PrivatePerson + Domestic land; Other named-deferred).

## Regulatory rule — NAV v3.0 customerVatStatus

### Source

NAV's `Online Számla` v3.0 XSDs are NOT vendored in-repo per ADR-0022's hand-rolled-validator posture (the validator is a structural recogniser, not an XSD parser). The authoritative source is the published NAV XSD bundle distributed with the Online Számla 3.0 documentation set. The schema element of record is:

- `InvoiceData/invoiceMain/invoice/invoiceHead/customerInfo/customerVatStatus`
- type: `customerVatStatusType` — `xs:simpleType` restriction on `xs:string`, enumeration of three tokens.

The closest in-repo evidence is the existing customer-info walker which has already been calibrated against the live NAV-test endpoint via the PR-77 ABORTED forensic (invoice 18, transaction `5E9KWQSOX3L9EC30`):

> `apps/aberp-ui/aberp.duckdb` seq=58 — `InvoiceAckStatus` payload, `response_xml` contains:
> `validationResultCode=ERROR`, `validationErrorCode=CUSTOMER_DATA_EXPECTED`,
> `message=Vevői adatok megadása kötelező, ha a vevő nem magánszemély.`
> (Customer data is mandatory if the buyer is not a natural person.)
> pointer: `customerInfo/customerVatStatus`, value: `DOMESTIC`.

That message — verbatim from NAV — names the three-way branch: "kötelező, ha a vevő **nem magánszemély**" (mandatory if the buyer is **not a natural person**). The negation is the closure: when the buyer IS a natural person, the data MUST NOT be carried.

### The three tokens

#### `DOMESTIC` — Hungarian business buyer

Requirements:

- `<customerVatStatus>DOMESTIC</customerVatStatus>` literal text body.
- `<customerVatData><customerTaxNumber>` block with structured children REQUIRED:
  - `<common:taxpayerId>` — 8 ASCII digits.
  - `<common:vatCode>` — 1 ASCII digit.
  - `<common:countyCode>` — 2 ASCII digits.
- `<customerAddress>` block REQUIRED (PR-77 — NAV business-rule `CUSTOMER_DATA_EXPECTED`).
- `<customerName>` REQUIRED.

This is the case ABERP ships today. The emit shape is byte-pinned by `emitter_writes_customer_address_under_domestic_status` (PR-77, `apps/aberp/tests/nav_xsd_validator_round_trip.rs`).

#### `PRIVATE_PERSON` — natural-person buyer (magánszemély)

Requirements:

- `<customerVatStatus>PRIVATE_PERSON</customerVatStatus>` literal text body.
- `<customerVatData>` **MUST NOT** appear. The element is `[0..1]` optional at the XSD level, but NAV's business-rule layer rejects the presence of customerVatData under PRIVATE_PERSON. This is the symmetric counterpart of `CUSTOMER_DATA_EXPECTED`.
- `<customerName>` REQUIRED — every invoice document carries a buyer name regardless of buyer kind.
- `<customerAddress>` — at the XSD level `[0..1]` optional. NAV's business-rule layer does NOT require it for PRIVATE_PERSON (the `CUSTOMER_DATA_EXPECTED` rule fires only when `customerVatStatus != PRIVATE_PERSON`, per the verbatim Hungarian message above). The Hungarian invoice law DOES require an address on the printed/recipient-facing document — but that is a print-and-PDF rule, not a NAV wire rule.

The validator's current PR-77 branch (`crates/nav-xsd-validator/src/validate.rs:466-481`) explicitly skips both checks under PRIVATE_PERSON:

```rust
let is_business_buyer = !status.eq_ignore_ascii_case("PRIVATE_PERSON");
if is_business_buyer {
    if !seen.contains(&"customerVatData") { return Err(MissingRequiredChild { … }); }
    if !seen.contains(&"customerAddress") { return Err(MissingRequiredChild { … }); }
}
```

What the validator does NOT yet check, and what this ADR adds (§5): **the symmetric `Forbidden` case** — `customerVatData` present under PRIVATE_PERSON.

#### `OTHER` — non-Hungarian buyer

Requirements:

- `<customerVatStatus>OTHER</customerVatStatus>` literal text body.
- `<customerVatData>` REQUIRED, with the inner shape being one of (XSD `xs:choice`):
  - `<communityVatNumber>` — EU community VAT number, free-form text per the NAV schema (the EU VAT-number registry validation lives at NAV's submit layer, not in ABERP).
  - `<thirdStateTaxId>` — non-EU tax identifier, free-form text.
- `<customerName>` REQUIRED.
- `<customerAddress>` REQUIRED (same `CUSTOMER_DATA_EXPECTED` rule that fires for DOMESTIC also fires for OTHER — the rule's predicate is `status != PRIVATE_PERSON`, not `status == DOMESTIC`).

The `customerTaxNumber` 3-element structured shape (with `common:` prefix) is **NOT** used for `OTHER`. The choice is between the two flat string elements.

### Hungarian tax-number authoritative shape

For the `DOMESTIC` branch the tax-number shape is already pinned by `parse_hungarian_tax_number` (`apps/aberp/src/nav_xml.rs`, called from `validate_supplier_info` and the validator's `walk_customer_tax_number`). PR-66 added the `common:` prefix invariant (`crates/nav-xsd-validator/src/validate.rs:540-547`). No changes to the DOMESTIC shape; this ADR adds the *conditional* on whether to emit it at all.

## Decision

### §1 — Closed-vocab enum `CustomerVatStatus`

A new closed-vocab enum, three variants, mirrored across:

- **Rust domain** — `aberp_billing::CustomerVatStatus` (preferred home: `modules/billing/src/domain/customer.rs` per ADR-0041's billing-module ownership; alternatively `apps/aberp/src/nav_xml.rs` next to `CustomerInfo` if billing has no buyer domain yet).
- **SPA wire mirror** — `apps/aberp-ui/ui/src/lib/api.ts::CustomerVatStatusBody` as a string union (`"PrivatePerson" | "Domestic" | "Other"`), serde-mirrored via PascalCase variant names (same shape as `PartnerKind`, `Currency`, `DeliveryDateOverride`).

```rust
pub enum CustomerVatStatus {
    PrivatePerson,  // → <customerVatStatus>PRIVATE_PERSON</…>
    Domestic,       // → <customerVatStatus>DOMESTIC</…>
    Other,          // → <customerVatStatus>OTHER</…>
}
```

Variant text MUST emit the SCREAMING_SNAKE NAV token (`PRIVATE_PERSON`), not the PascalCase Rust name (`PrivatePerson`). Pinned by an emit test asserting verbatim bytes (mirror of the PR-77 emit pin).

**v1 scope:** PrivatePerson + Domestic ship as fully wired-through. Other is named in the enum (so a SPA wire body carrying `"Other"` deserialises without `Err`) but every surface that materialises the variant either:
- emits the partial wire shape with a `TODO(ADR-0048-§Other)` marker that loud-fails at emit time, OR
- the UI hides the Külföldi radio option behind a feature flag (ADR-0046's `OWN` precedent),
- the preflight surfaces a typed `CustomerVatStatusOtherNotSupported` error.

§7 names the chosen approach.

### §2 — Data-model: per-partner vat_status + nullable tax_number

**Recommendation: vat_status is a property of the partner record (storage), surfaced on the invoice wire body (audit-trail).**

The choice between per-partner and per-invoice storage was non-trivial. Ervin's brief asked for the recommendation + reasoning:

#### Why per-partner

A given buyer entity is **intrinsically** one of the three kinds. "AZ9 Services Kft." cannot be a natural person on Tuesday and a business on Wednesday. "Kovács János" (private buyer) never carries an ADÓSZÁM regardless of which invoice references him. Modelling vat_status on `partners` instead of per-invoice:

1. **Type-stability.** The data layer pins the buyer's kind once. A partner record's invariant ("vat_status = Domestic → tax_number IS NOT NULL") becomes a CHECK constraint and a runtime guard, not a per-invoice condition.
2. **UI flow already partner-driven.** PR-77 established the pattern: the IssueInvoice form's customer block is populated from the partner combobox (`apps/aberp-ui/ui/src/routes/IssueInvoice.svelte` `pickPartner` / `buyerFieldsFromPartner`). Adding vat_status to the partner record continues that pattern — the radio at issuance-time DISPLAYS the partner's stored vat_status but does NOT let the operator override it. The vat-status choice lives where the buyer's identity lives: the partner form.
3. **One source of truth for the tax-number-required rule.** The partner form's `validate_partner_inputs` becomes the single gate on whether tax_number is required. Pre-PR-96 the rule is "always required"; post-PR-96 it's "required iff vat_status == Domestic." The invoice preflight then inherits the partner's status — no per-invoice override means no per-invoice conditional logic on the issuance side.
4. **Mirrors ADR-0046 §2 / ADR-0040 §addendum.** Master-data concerns (kind, status, address) live on the master-data entity (`partners`, `products`, `seller_banks`). The invoice is the *application* of master-data plus per-invoice content (lines, dates, currency, notes).

#### What the wire body still carries

The invoice wire body STILL carries `customer.vatStatus` as a per-invoice field. Reasons:

1. **Audit-trail integrity.** PR-77's address-from-partner pattern denormalised the address quartet onto the wire body so the operator-twin record of "what was issued" matches what the partner record looked like **at that issuance time**. A future edit to the partner (renaming, address correction) doesn't retroactively change the wire bytes already on disk + NAV-side. Same logic applies to vat_status: stamping the per-invoice wire body locks the value as-of-issuance.
2. **Ad-hoc issuance (named-deferred).** A future "issue invoice to a one-shot buyer without a partner record" flow needs the vat_status field on the wire even when no partner row exists. Modelling it per-invoice today keeps the surface ready.

The path is therefore: **partner table stores the canonical vat_status → IssueInvoice form reads it from the picked partner → composes it onto the wire body → backend stamps it onto the audit payload → emitter conditionally renders the customerVatStatus / customerVatData / customerAddress trio**.

#### DuckDB migration (idempotent, `ADD COLUMN IF NOT EXISTS`)

```sql
-- partners.customer_vat_status — closed-vocab enum with CHECK + DEFAULT.
ALTER TABLE partners ADD COLUMN IF NOT EXISTS customer_vat_status VARCHAR
    NOT NULL DEFAULT 'Domestic'
    CHECK (customer_vat_status IN ('PrivatePerson', 'Domestic', 'Other'));

-- partners.tax_number — drop NOT NULL. Application-level invariant takes over:
-- vat_status = 'Domestic' → tax_number IS NOT NULL AND matches xxxxxxxx-y-zz.
-- vat_status = 'PrivatePerson' → tax_number IS NULL (or empty-after-trim).
-- vat_status = 'Other' → tax_number IS NULL; community_vat_number OR third_state_tax_id is populated instead.
ALTER TABLE partners ALTER COLUMN tax_number DROP NOT NULL;
```

The migration backfills existing partners as `Domestic` so no existing data shifts meaning (rule 11 — match codebase conventions; rule 12 — fail loud, not silent reinterpretation). The `tax_number` column stays VARCHAR (no rename, no width change) so the change is purely a constraint relaxation. Existing reads continue to work; new writes for PrivatePerson rows insert `NULL` (or an empty string trimmed to NULL — the read path must treat both identically per CLAUDE.md rule 7).

**Foreign-buyer columns (named-deferred to v2):** when `Other` lands, two additional nullable columns join the partners table — `community_vat_number VARCHAR` and `third_state_tax_id VARCHAR`. Mutually-exclusive at the application layer (a single Other partner has exactly one of the two populated). The migration is idempotent and lands with the v2 PR; no v1 SQL beyond the two ALTERs above.

#### Rust domain shape

```rust
#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct Partner {
    pub id: String,
    pub display_name: String,
    pub legal_name: String,
    pub kind: PartnerKind,
    /// PR-96 / ADR-0048 — closed vocab. `Domestic` for pre-PR-96 rows
    /// per the backfill. Drives whether `tax_number` is required.
    pub customer_vat_status: CustomerVatStatus,
    /// PR-96 / ADR-0048 — nullable when `customer_vat_status` is
    /// `PrivatePerson` (NULL) or `Other` (NULL — replaced by
    /// community_vat_number / third_state_tax_id, both v2-deferred).
    pub tax_number: Option<String>,
    pub eu_vat_number: Option<String>,
    // … (address quartet + bank_account + contacts unchanged from PR-77)
}
```

`PartnerInputs` mirrors the same shape (`tax_number: Option<String>`, `customer_vat_status: CustomerVatStatus`).

### §3 — UI spec (radio + conditional inputs)

#### Buyer block on the IssueInvoice form (`apps/aberp-ui/ui/src/routes/IssueInvoice.svelte:750-815`)

Inserted ABOVE the existing ADÓSZÁM input row:

```
┌──────────────────────────────────────────────────────────┐
│ Vevő típusa / Buyer type                                 │
│  ( ) Adóalany / Domestic business                         │
│  ( ) Magánszemély / Natural person                         │
│  ( ) Külföldi / Foreign           [v2 — named-deferred]    │
└──────────────────────────────────────────────────────────┘
```

The radio binds to `form.customerVatStatus` (defaults to `"Domestic"`, mirroring the partner-row default). When the operator picks a partner via the combobox, `pickPartner` overwrites `form.customerVatStatus` with the partner's stored status. The radio remains interactable so an ad-hoc override at issue time is possible (the choice is captured on the audit payload regardless).

The ADÓSZÁM input row (currently always-required at line 750-767) becomes conditional:

- `Domestic` — input enabled + `required`. Same posture as today.
- `PrivatePerson` — input is rendered but **disabled** (`disabled` attribute on the `<input>`, `aria-disabled="true"`, greyed-out via existing `:disabled` CSS). The bound value is forced to empty by an effect on the radio change. A small hint replaces the inline error:

  > **Magánszemély vevő esetén nem kell adószám.**
  > *No tax number is required for a natural-person buyer.*

- `Other` — input is rendered as today **BUT** v1 named-defers this branch. The composer (`composeIssueInvoiceBody`) emits a `customerVatStatusOtherNotSupported` preflight-style error before the body is built; the SPA renders an inline message: "Külföldi vevő kibocsátása későbbi PR-ben (ADR-0048 §7). / Foreign-buyer issuance lands in a later PR."

Keeping the input **disabled-not-removed** (rather than vanishing it) is deliberate — the layout shift on radio change would jar the operator, and the field staying visible signals "the system knows this is a tax-number slot, it just doesn't apply for this buyer kind."

The customer address quartet (PR-77 — Country / Postal / City / Street) stays REQUIRED for `Domestic` (PR-77 hold) and BECOMES OPTIONAL for `PrivatePerson` per NAV's business rule. The Svelte fieldset switches the four `required` flags off when `form.customerVatStatus == "PrivatePerson"`. The printed PDF still renders the address fields if the operator chose to fill them — the PDF model already accommodates blank address fields (PR-77's `customerAddress` is `Option<CustomerAddress>` on the Rust side per `apps/aberp/src/nav_xml.rs:253`).

#### Partner form (the partner CRUD `PartnerForm.svelte`)

The same three-option radio at the top of the form. Below the radio:

- `Domestic` selected → ADÓSZÁM input is `required`, validated against `validate_tax_number`'s `xxxxxxxx-y-zz` shape.
- `PrivatePerson` selected → ADÓSZÁM input is disabled + greyed; same hint as the IssueInvoice form. The partner record stores `tax_number: NULL`.
- `Other` selected (v2) → the input morphs into a label-toggle "EU community VAT" vs "Non-EU third-state tax ID" with a single text input bound to the chosen sub-shape.

The partner-form validation (`validate_partner_inputs` in `apps/aberp/src/partners.rs:294`) becomes:

```rust
match inputs.customer_vat_status {
    CustomerVatStatus::Domestic => {
        // Existing PR-48α validation.
        if let Err(msg) = validate_tax_number(inputs.tax_number.as_deref().unwrap_or("")) {
            errors.push(ValidationError { field: "tax_number", message: msg });
        }
    }
    CustomerVatStatus::PrivatePerson => {
        // Forbid a non-empty tax_number — a private buyer with an
        // ADÓSZÁM is operator confusion, surface it loud.
        if inputs.tax_number.as_deref().is_some_and(|s| !s.trim().is_empty()) {
            errors.push(ValidationError {
                field: "tax_number",
                message: "magánszemély vevőhöz nem tartozhat adószám".into(),
            });
        }
    }
    CustomerVatStatus::Other => {
        // v2 — community_vat_number XOR third_state_tax_id required.
        // Named-deferred per ADR-0048 §7. v1 fires a generic error
        // pointing at the radio.
    }
}
```

### §4 — NAV emit changes (`apps/aberp/src/nav_xml.rs:982-1007`)

`CustomerInfo` gains a `customer_vat_status: CustomerVatStatus` field and the `tax_number` field becomes `Option<String>`:

```rust
pub struct CustomerInfo {
    pub customer_vat_status: CustomerVatStatus,
    pub tax_number: Option<String>,
    pub name: String,
    pub address: Option<CustomerAddress>,  // PR-77 — unchanged shape.
}
```

`write_customer` becomes conditional:

```rust
fn write_customer(w: &mut Writer<&mut Vec<u8>>, c: &CustomerInfo) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("customerInfo")))?;
    text_element(w, "customerVatStatus", c.customer_vat_status.as_nav_token())?;

    match c.customer_vat_status {
        CustomerVatStatus::Domestic => {
            let parsed = parse_hungarian_tax_number(
                c.tax_number.as_deref()
                    .ok_or_else(|| anyhow!("Domestic buyer requires tax_number — invariant violated upstream"))?
            )?;
            // existing PR-50 structured emit unchanged
            w.write_event(Event::Start(BytesStart::new("customerVatData")))?;
            w.write_event(Event::Start(BytesStart::new("customerTaxNumber")))?;
            common_element(w, "taxpayerId", &parsed.taxpayer_id)?;
            common_element(w, "vatCode", &parsed.vat_code)?;
            common_element(w, "countyCode", &parsed.county_code)?;
            w.write_event(Event::End(BytesEnd::new("customerTaxNumber")))?;
            w.write_event(Event::End(BytesEnd::new("customerVatData")))?;
        }
        CustomerVatStatus::PrivatePerson => {
            // INTENTIONALLY emit no customerVatData. NAV business-rule
            // forbids it under PRIVATE_PERSON.
        }
        CustomerVatStatus::Other => {
            // v2 — community_vat_number XOR third_state_tax_id emit.
            // Loud-fail in v1 per ADR-0048 §7.
            return Err(anyhow!("ADR-0048 §7: Other-status customer emit is named-deferred"));
        }
    }

    text_element(w, "customerName", &c.name)?;
    if let Some(address) = c.address.as_ref() {
        write_customer_address(w, address)?;
    }
    w.write_event(Event::End(BytesEnd::new("customerInfo")))?;
    Ok(())
}
```

The `parse_hungarian_tax_number` invariant violation in the Domestic branch is upstream-blocked by the new preflight rule + the partner-form validation; reaching that arm with `tax_number: None` is a programmer error and surfaces as a structured `anyhow!` per CLAUDE.md rule 12.

The PR-77 byte-verbatim emit pin (`emitter_writes_customer_address_under_domestic_status`) continues to pass for Domestic. A new sibling pin lands for PrivatePerson:

```rust
#[test]
fn emitter_writes_customer_info_under_private_person_omits_vat_data() {
    let bytes = render_minimal_invoice_with_private_person_buyer();
    let s = std::str::from_utf8(&bytes).unwrap();

    assert!(s.contains("<customerVatStatus>PRIVATE_PERSON</customerVatStatus>"));
    assert!(!s.contains("<customerVatData>"),
        "PRIVATE_PERSON buyer must NOT carry customerVatData");
    assert!(!s.contains("<customerTaxNumber>"),
        "PRIVATE_PERSON buyer must NOT carry customerTaxNumber");
    assert!(s.contains("<customerName>"));
}
```

### §5 — Validator changes (`crates/nav-xsd-validator/src/validate.rs:403-488`)

The PR-77 PrivatePerson branch ALREADY handles the "do not require" half correctly (lines 466-481). The ADR-0048 strengthening adds the **symmetric Forbidden** check: customerVatData MUST NOT appear under PRIVATE_PERSON.

```rust
Event::End(_) => {
    check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
    let status = customer_vat_status.as_deref().unwrap_or("");
    let is_business_buyer = !status.eq_ignore_ascii_case("PRIVATE_PERSON");

    if is_business_buyer {
        // PR-77 hold — unchanged.
        if !seen.contains(&"customerVatData") {
            return Err(NavXsdValidationError::MissingRequiredChild {
                parent: PARENT, expected: "customerVatData",
            });
        }
        if !seen.contains(&"customerAddress") {
            return Err(NavXsdValidationError::MissingRequiredChild {
                parent: PARENT, expected: "customerAddress",
            });
        }
    } else {
        // PR-96 / ADR-0048 §5 — new symmetric rule. The validator
        // models NAV's "customerVatData forbidden under PRIVATE_PERSON"
        // business-rule locally, so the trap door cannot reopen.
        if seen.contains(&"customerVatData") {
            return Err(NavXsdValidationError::ForbiddenChildUnderStatus {
                parent: PARENT,
                element: "customerVatData",
                status: "PRIVATE_PERSON",
            });
        }
    }
    return Ok(());
}
```

The new error variant joins `NavXsdValidationError` (`crates/nav-xsd-validator/src/error.rs`):

```rust
/// PR-96 / ADR-0048 §5 — a child element appeared inside `parent` that
/// is forbidden under the captured `status` value. Distinct from
/// `UnexpectedElement` because the element IS in the parent's allowlist
/// for SOME values of the discriminant — just not for `status`.
///
/// Today fires only on `customerVatData` under PRIVATE_PERSON. The
/// generic shape is forward-compatible with a future rule (e.g. an
/// OTHER-status buyer forbidding `customerTaxNumber` inside
/// `customerVatData`).
#[error("forbidden child <{element}> inside <{parent}> when status is `{status}`")]
ForbiddenChildUnderStatus {
    parent: &'static str,
    element: &'static str,
    status: &'static str,
},
```

New validator pins:

- `private_person_status_forbids_customer_vat_data_in_input` — feeds a body with `customerVatStatus = PRIVATE_PERSON` AND `customerVatData` present; asserts `ForbiddenChildUnderStatus`.
- `private_person_status_accepts_missing_customer_vat_data_and_address` — PR-77's existing pin, renamed for clarity; verifies the PRIVATE_PERSON branch passes when the optional elements are absent.
- `domestic_status_still_requires_vat_data_and_address` — PR-77 hold; pinned again here to guard against a future merge collapsing the two branches.

The PR-77 fixture `MIN_VALID` stays Domestic. A new fixture `MIN_VALID_PRIVATE_PERSON` lands with PrivatePerson + customerName + (optionally) customerAddress.

### §6 — Preflight changes (`apps/aberp/src/issue_preflight.rs:82-168, 434-485`)

`IssueInvoiceRequest.customer` (in `apps/aberp/src/serve.rs::CustomerJson`) gains:

```rust
#[derive(Deserialize)]
pub struct CustomerJson {
    pub name: String,
    /// PR-96 / ADR-0048 — closed vocab; backwards-compat via #[serde(default)].
    /// Pre-PR-96 wire bodies omit the field and the deserializer defaults
    /// to `Domestic` (matches the pre-PR-96 implicit behaviour).
    #[serde(default = "default_customer_vat_status")]
    pub vat_status: CustomerVatStatus,
    /// Pre-PR-96 shape — universally required. Post-PR-96 required iff
    /// vat_status == Domestic; PrivatePerson buyers omit it.
    pub tax_number: String,  // wire body keeps String (empty-after-trim = absent)
    #[serde(default)]
    pub address: Option<AddressJson>,  // PR-77 hold.
}
fn default_customer_vat_status() -> CustomerVatStatus { CustomerVatStatus::Domestic }
```

`InvoicePreflightError` adds one new variant for the v1-shipping branch + one for the v2 named-deferred branch:

```rust
pub enum InvoicePreflightError {
    // … existing variants …

    /// PR-96 / ADR-0048 §6 — `customer.vat_status == PrivatePerson` AND
    /// the operator supplied a non-empty `customer.tax_number`. NAV
    /// rejects PRIVATE_PERSON + customerVatData; rather than emit and
    /// burn the sequence, surface inline at preflight.
    CustomerTaxNumberPresentForPrivatePerson { actual: String },

    /// PR-96 / ADR-0048 §7 — `customer.vat_status == Other`. v1 ABERP
    /// does not yet implement the EU community VAT / non-EU third-state
    /// tax-id branch; the preflight rejects this body shape with a
    /// pointer at the radio so the operator knows the branch is
    /// known-not-implemented (rather than silently breaking at the NAV
    /// submit layer or producing a malformed wire body).
    CustomerVatStatusOtherNotSupported,
}
```

The existing `CustomerTaxNumberMissing` and `CustomerTaxNumberMalformed` variants become **conditional on `vat_status == Domestic`**:

```rust
match request.customer.vat_status {
    CustomerVatStatus::Domestic => {
        // Existing PR-69 + PR-77 logic.
        let tax_trimmed = request.customer.tax_number.trim();
        if tax_trimmed.is_empty() {
            errors.push(InvoicePreflightError::CustomerTaxNumberMissing);
        } else {
            match parse_hungarian_tax_number(tax_trimmed) { … }
        }
        // PR-77 address-required check stays.
        if tax_number_well_formed { /* require address */ }
    }
    CustomerVatStatus::PrivatePerson => {
        // PR-96 — opposite invariant: tax_number MUST be empty.
        let tax_trimmed = request.customer.tax_number.trim();
        if !tax_trimmed.is_empty() {
            errors.push(InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson {
                actual: tax_trimmed.to_string(),
            });
        }
        // Address is optional under PRIVATE_PERSON — no fire.
    }
    CustomerVatStatus::Other => {
        errors.push(InvoicePreflightError::CustomerVatStatusOtherNotSupported);
    }
}
```

Customer name check (CustomerNameEmpty) stays universal — every buyer kind requires a name.

The `field_path` for the two new variants:

- `CustomerTaxNumberPresentForPrivatePerson` → `customer.taxNumber` (targets the disabled-input field; the SPA renderer surfaces the error as a banner above the radio because the input is disabled and can't receive focus).
- `CustomerVatStatusOtherNotSupported` → `customer.vatStatus` (targets the radio).

Bilingual messages (`message_hu` / `message_en`) per ADR-0038's pattern.

New preflight pins (cargo test in `apps/aberp/src/issue_preflight.rs::tests`):

- `fires_customer_tax_number_present_for_private_person`
- `does_not_fire_customer_tax_number_missing_for_private_person`
- `does_not_fire_customer_address_missing_for_private_person`
- `fires_customer_vat_status_other_not_supported`
- The existing PR-69 fires-pins all add an explicit `vat_status: Domestic` to their fixtures (mechanical update).

### §7 — `Other` (foreign buyer) — v1 scope decision

**v1 ships PrivatePerson + Domestic only. `Other` is named-deferred to v2 (a follow-on PR).**

Rationale:

1. The brief's UX proposal names three radio options but explicitly allows "Other can be deferred (named) if EU+non-EU foreign is complex." The EU vs non-EU distinction adds three storage columns (community_vat_number, third_state_tax_id), a sub-radio in the partner form, NAV-side validation of EU VIES format, and a separate validator branch. None of those changes meaningfully without seeing the v1 PrivatePerson surface land first.
2. CLAUDE.md rule 2 — minimum code. Adding both branches in one PR risks shipping the rarely-exercised Other branch broken.
3. The closed-vocab enum has the `Other` variant from day one (§1), so v2 is purely additive: a partner row with `vat_status = Other` cannot be created in v1 because the SPA's PartnerForm hides the radio option; a wire body carrying `Other` deserialises but trips `CustomerVatStatusOtherNotSupported` at preflight; the emitter loud-fails per the explicit `anyhow!` in §4. None of those v1 surfaces are wrong — they're explicit-not-yet markers.

v2 PR's named acceptance criteria:

- Partner radio surfaces "Külföldi / Foreign" option.
- Partner form sub-radio "EU community VAT" vs "Non-EU third-state ID" with a single text input.
- DuckDB migration `MIGRATE_PR_96B_SQL`: `ALTER TABLE partners ADD COLUMN community_vat_number VARCHAR; ALTER TABLE partners ADD COLUMN third_state_tax_id VARCHAR;`.
- `parse_hungarian_tax_number` is NOT called on Other buyers (different shape).
- NAV emit: `<customerVatData>` carries `<communityVatNumber>` OR `<thirdStateTaxId>` (not the structured `<customerTaxNumber>` block).
- Validator: a new `walk_customer_vat_data_other_branch` handles the EU/non-EU choice. The existing `walk_customer_vat_data` (PR-66, line 490-494) is renamed to `walk_customer_vat_data_domestic_branch` for clarity, and the parent `walk_customer_info` dispatches by status.
- Preflight: `CustomerVatStatusOtherNotSupported` is removed; new variants `CommunityVatNumberMissing`, `ThirdStateTaxIdMissing`, `OtherBuyerTaxIdAmbiguous` (both populated) land.

### §8 — Audit + storage

The vat_status choice is **part of the issue payload** and MUST be auditable per ADR-0008's tamper-evident posture. `InvoiceDraftCreatedPayload` (`apps/aberp/src/audit_payloads.rs:126-237`) gains one new field:

```rust
pub struct InvoiceDraftCreatedPayload {
    // … existing PR-18/PR-44γ/PR-73/PR-82/PR-84 fields …

    /// PR-96 / ADR-0048 — closed-vocab buyer kind at issuance time.
    /// `Some("Domestic")` / `Some("PrivatePerson")` for post-PR-96
    /// entries; `None` for pre-PR-96 entries (read path treats `None`
    /// and `Some("Domestic")` identically — the pre-PR-96 implicit
    /// posture). `Some("Other")` lands with v2 (ADR-0048 §7); inspector
    /// recovery on an unrecognised string indicates ledger tampering or
    /// forward-compat schema drift.
    #[serde(default)]
    pub customer_vat_status: Option<String>,
}
```

Stamped via a new builder method `with_customer_vat_status(status: CustomerVatStatus)` on `InvoiceDraftCreatedPayload` (mirror of `with_invoice_dates`, `with_notes`, `with_bank_snapshot`).

**F12 ritual triggers.** Per the audit-payloads header (`apps/aberp/src/audit_payloads.rs:25-33`):

> Adding a field is backward-compatible (older readers see the old shape via `#[serde(default)]` if they choose to parse). Removing a field or changing a field's semantic shape requires a new EventKind variant.

A new `Option<String>` field with `#[serde(default)]` is the additive shape — the F12 four-edit ritual does NOT fire. The InvoiceDraftCreated kind keeps its existing name. The pre-PR-96 round-trip pin (`draft_created_round_trip`) continues to pass with `customer_vat_status: None`; a new sibling pin `draft_created_round_trip_with_customer_vat_status` covers the post-PR-96 shape.

**Storno + modification chains.** The chain operations (`issue_storno.rs`, `issue_modification.rs`) carry the base invoice's `customer_vat_status` through via the side-stored `input.json`. Bases issued pre-PR-96 have `customer_vat_status: None` in the side-store; the chain operation defaults to `Domestic` (matches pre-PR-96 behaviour) so the chain XML matches the base's wire shape. Bases issued post-PR-96 for PrivatePerson buyers carry the status forward verbatim — the storno's `<customerInfo>` mirrors the base's PRIVATE_PERSON shape.

## Concrete implementation checklist (the future PR brief)

Land in dependency order. Each step cites the file + the PR's purpose.

1. **[Domain] Closed-vocab enum.** Add `CustomerVatStatus` to `apps/aberp/src/nav_xml.rs` (or `modules/billing/src/domain/customer.rs` if the billing module gains a buyer-info domain). Three variants. Serde PascalCase mirror. `as_nav_token()` → `"PRIVATE_PERSON"` / `"DOMESTIC"` / `"OTHER"`. Pin: `customer_vat_status_serde_round_trip` (mirror of `partner_kind_serde_round_trip_pin`).

2. **[DuckDB] Partner migration.** `apps/aberp/src/partners.rs:325-336` (`PARTNERS_SCHEMA_SQL`). Add the two ALTER statements (§2). Idempotent. Pin: `partners_migration_adds_customer_vat_status_column`.

3. **[Domain] Partner struct shape.** `apps/aberp/src/partners.rs:148-172` (`Partner`) + `:179-201` (`PartnerInputs`). `tax_number: String` → `Option<String>`. Add `customer_vat_status: CustomerVatStatus`. Update `row_to_partner` (`:643-680`) to read the new column + handle nullable tax_number. Update `create_partner` / `update_partner` insert/update binds (`:416-447`, `:585-606`).

4. **[Domain] Partner validation.** `apps/aberp/src/partners.rs:294-319` (`validate_partner_inputs`). Switch on `customer_vat_status`; per §3 the tax-number rule is conditional. Three new pins per branch.

5. **[Backend] Wire-body shape.** `apps/aberp/src/serve.rs::CustomerJson` (and the storno / modification counterparts). Add `vat_status` (serde-defaulted to Domestic for back-compat). Pin: the existing `serve_issue_route` integration test fires a Domestic body — unchanged. New integration test fires a PrivatePerson body end-to-end.

6. **[Backend] Domain hydration.** `apps/aberp/src/issue_invoice.rs` — `CustomerInfo.customer_vat_status` populated from `CustomerJson.vat_status`. `tax_number: Option<String>` flows through.

7. **[Backend] Preflight.** `apps/aberp/src/issue_preflight.rs:434-485` — switch on `vat_status` per §6. Two new variants (`CustomerTaxNumberPresentForPrivatePerson`, `CustomerVatStatusOtherNotSupported`) added to the closed enum + every accessor (`kind`, `field_path`, `message_hu`, `message_en`). Six new pins per §6.

8. **[Backend] NAV emit.** `apps/aberp/src/nav_xml.rs:982-1007` — `write_customer` becomes conditional per §4. New emit pin `emitter_writes_customer_info_under_private_person_omits_vat_data` mirrors PR-77's domestic pin.

9. **[Validator] Symmetric `Forbidden` rule.** `crates/nav-xsd-validator/src/validate.rs:466-487` — add the PRIVATE_PERSON-forbids-customerVatData check per §5. `crates/nav-xsd-validator/src/error.rs` — new `ForbiddenChildUnderStatus` variant. Three new pins per §5.

10. **[Backend] Audit payload.** `apps/aberp/src/audit_payloads.rs:126-237` — add `customer_vat_status: Option<String>` field. `with_customer_vat_status(...)` builder. Issue-pipeline call sites stamp the field. Pin: `draft_created_round_trip_with_customer_vat_status` (post-PR-96 shape) AND `draft_created_round_trip` (pre-PR-96 shape via `None`).

11. **[SPA] Wire mirror.** `apps/aberp-ui/ui/src/lib/api.ts` — add `CustomerVatStatusBody` string union AND `customer.vatStatus` + nullable `customer.taxNumber` fields. Mirror in `partners.ts::Partner` + `PartnerInputs`.

12. **[SPA] Partner form.** `apps/aberp-ui/ui/src/routes/PartnerForm.svelte` — add the three-option radio + condition the ADÓSZÁM input. Hide the "Külföldi" option behind a feature flag OR show-but-disabled with the v2 hint. Vitest pin: form composes the right wire body for PrivatePerson; tax_number field stays disabled.

13. **[SPA] BuyerFields + composer.** `apps/aberp-ui/ui/src/lib/partners.ts::buyerFieldsFromPartner` — populate `customerVatStatus` from the partner's stored field. `apps/aberp-ui/ui/src/lib/issue-invoice.ts::composeIssueInvoiceBody` — emit `customer.vatStatus` + condition `customer.taxNumber` (omit when PrivatePerson). Three Vitest pins per branch.

14. **[SPA] IssueInvoice form.** `apps/aberp-ui/ui/src/routes/IssueInvoice.svelte:750-815` — add the radio fieldset ABOVE the existing customer block. Condition the ADÓSZÁM input's `disabled` + `required` flags. Replace the inline error with the bilingual hint for PrivatePerson. Same surgical edit on `ModificationInvoice.svelte`.

15. **[Tests] Existing fixtures migrate.** Every `CustomerInfo` / `CustomerJson` / `Partner` literal in the test tree gains `customer_vat_status: CustomerVatStatus::Domestic` (preserves pre-PR-96 behaviour for the existing pins). PR-77 listed nine test files for the address quartet; this PR adds the same mechanical sweep for the vat_status field.

16. **[Docs] ADR-0048 status + handoff.** Flip this ADR's status from `Proposed` to `Accepted — PR-XX (date)`. Land alongside `_handoffs/PR-XX-handoff.md` and `_handoffs/PR-XX-commit-message.txt`.

17. **[Memory] reference_nav_gotchas.md.** Confirm the §"PRIVATE_PERSON forbids customerVatData" entry exists and points at this ADR + the new validator pin.

Step ordering is deliberate: domain → storage → wire → backend logic → emit → validator → audit → SPA. Each step has a self-contained pin so a regression at step N surfaces at step N's test layer, not as a downstream NAV-side ABORTED.

## Alternatives considered

### A1. Per-invoice vat_status only (no partner-side field)

Rejected. The buyer's kind is intrinsic to the entity, not the invoice. Modelling it per-invoice forces the operator to re-pick the radio at every issuance for the same partner — a UX trap (operator picks AZ9 Services → radio defaults to Domestic implicitly → operator forgets to verify → wrong vat_status stamped). Per-partner storage with per-invoice wire stamping captures the "as-of-issuance" snapshot without the UX trap.

### A2. Drop tax_number's NOT NULL constraint AND drop the partner-level vat_status (rely on emptiness-of-tax-number to imply PrivatePerson)

Rejected. Encoding the discriminant via field-emptiness is the worst class of closed-vocab violation (CLAUDE.md rule 7). An empty tax_number could mean "Hungarian business whose tax number wasn't entered yet" (data-quality gap) OR "intentional PrivatePerson buyer" — these are semantically different and must not collapse to one wire shape. The explicit `customer_vat_status` discriminant pins intent.

### A3. Ship Other (foreign buyer) in v1 alongside PrivatePerson

Rejected per §7. EU vs non-EU is a meaningful sub-branch with its own NAV wire shape (`<communityVatNumber>` vs `<thirdStateTaxId>`), its own validator rule, and its own storage columns. Shipping it in the same PR as PrivatePerson would triple the test surface and risk shipping the rarely-exercised branch broken. v1 closes the visible UX gap (PrivatePerson); v2 adds Other when the operator surfaces the first foreign-buyer issuance.

### A4. Make the customer-info wire body's `vat_status` field non-optional (no `#[serde(default)]`)

Rejected. Pre-PR-96 invoice bodies on disk (the side-stored `input.json` files at `~/.aberp/<tenant>/issued/<ULID>.input.json`) omit the field entirely. Storno + modification chains read these files; a `#[serde(default)]` annotation is the standard backwards-compat shape per ADR-0042's note-field precedent. The default explicitly maps to `Domestic` (the pre-PR-96 implicit posture) so chain operations on pre-PR-96 bases continue to emit Domestic wire bodies.

## Consequences

- **Pro:** The PRIVATE_PERSON buyer surface is closed end-to-end. NAV's `Vevői adatok megadása kötelező, ha a vevő nem magánszemély.` message can no longer trap a future natural-person invoice — the local validator + preflight model the rule symmetrically.
- **Pro:** The `customer_vat_status` discriminant is the single source of truth for whether `tax_number` is required, replacing the implicit "always required" assumption with an explicit closed-vocab gate.
- **Pro:** Foreign-buyer issuance has a named slot ready (the `Other` variant + the v2 acceptance criteria in §7); when Ervin needs to invoice a Vienna client, the v2 PR has a clean surface to extend rather than a "now where do I put the EU VAT number" debate.
- **Pro:** Audit-trail integrity preserved per ADR-0008 — every post-PR-96 invoice's `InvoiceDraftCreated` payload carries `customer_vat_status` verbatim. Pre-PR-96 entries deserialise cleanly with `None` (treated as Domestic on read).
- **Con:** Wide surface change. 17-step implementation checklist touching domain, storage, wire, backend, emit, validator, audit, SPA, and tests. The existing PR-77 9-file test fixture sweep is a precedent for the mechanical-update scope — same shape, larger blast radius.
- **Con:** A partner row carrying `vat_status = PrivatePerson` + a populated tax_number is an inconsistent state the partner-form validation must catch. The data layer's CHECK constraint cannot enforce conditional NOT NULL; the invariant lives at the application layer (`validate_partner_inputs`) AND a SQL `CHECK ((customer_vat_status = 'Domestic' AND tax_number IS NOT NULL) OR (customer_vat_status <> 'Domestic'))` is added if DuckDB tolerates the shape — verify at v1 PR time.
- **Con:** The disabled-not-removed input is a small UX paper-cut; some operators may attempt to type into the disabled field. The bilingual hint mitigates but doesn't eliminate this — a screenshot review with Ervin during v1 PR is named.

## Pin tests this ADR commits to (v1 PR)

Counted across files, the v1 PR adds ~20 pins:

- `apps/aberp/src/nav_xml.rs::tests` — 1 (closed-vocab round-trip).
- `apps/aberp/src/partners.rs::tests` — 4 (vat_status backfill default, conditional tax-number validation per branch, PrivatePerson rejects non-empty tax_number).
- `apps/aberp/src/issue_preflight.rs::tests` — 6 (per §6).
- `apps/aberp/src/audit_payloads.rs::tests` — 2 (pre-PR-96 + post-PR-96 round-trips).
- `crates/nav-xsd-validator/src/validate.rs::tests` — 3 (per §5).
- `apps/aberp-ui/ui/src/lib/partners.test.ts` — 2 (radio binding, conditional ADÓSZÁM disable).
- `apps/aberp-ui/ui/src/lib/issue-invoice.test.ts` — 3 (composer per branch).
- Integration: `apps/aberp/tests/serve_issue_route.rs` — 1 (end-to-end PrivatePerson POST → 200, wire-body assert customerVatData absent, audit payload carries `customer_vat_status: Some("PrivatePerson")`).

## Open questions for Ervin

These are the points the v1 build PR will need explicit answers on; they are NOT blocking this ADR.

1. **Per-invoice override vs partner-locked.** Recommended: the IssueInvoice radio remains interactable so an ad-hoc override at issue time is possible (`§3`). Alternative: lock the radio when a partner is picked (read-only display, edit-only on "ad-hoc buyer" mode). The lock-on-pick posture is more defensive against operator error; the interactable posture is more flexible. Pick one.
2. **Storno / modification chain default for pre-PR-96 bases.** The chain operation defaults to Domestic when reading a side-stored `input.json` that lacks `vat_status`. Is that the desired posture, or should the operator be prompted to confirm the status at chain-issue time? (Pre-PR-96 bases are guaranteed Domestic; this is a no-op for them, but the question matters if Ervin wants the chain-issue flow to surface the radio for v2 Other parents.)
3. **DuckDB CHECK constraint on the conditional invariant.** Confirm DuckDB tolerates `CHECK ((customer_vat_status = 'Domestic' AND tax_number IS NOT NULL) OR …)` — if not, the invariant lives only at the application layer + a runtime guard.
4. **PartnerForm sub-radio for v2.** When `Other` ships, is the EU vs non-EU distinction a sub-radio or a free-text field with auto-classification? (Free-text + classify-on-save loses operator-visible intent; sub-radio matches the ADR-0046 closed-vocab posture.) v2 PR decides; flagged here.
5. **Printed-PDF address for PrivatePerson.** NAV's wire rule does not require `<customerAddress>` under PRIVATE_PERSON, but Hungarian invoice law DOES require an address on the printed document. The PDF model already handles `Option<CustomerAddress>` — should the PartnerForm REQUIRE the address quartet for PrivatePerson buyers (to satisfy the print rule even though the wire rule doesn't), or leave it optional? Default recommendation: keep optional in v1, surface as a "fill in for the PDF" hint; revisit if a printed invoice ships without an address.

## Amendment 2026-05-29 (Session 150) — Override 2 PDF half reverted

Override 2 in the original ADR omitted name + address from the PDF for PRIVATE_PERSON
buyers on GDPR-conservative grounds. Hungarian ÁFA Act §169 (research captured in
Session 129 memo) mandates buyer name AND address on the printed/PDF invoice for
ALL customer types including natural persons. GDPR Art. 6(1)(c) legal-obligation
basis covers the collection precisely because §169 mandates it — no GDPR conflict.

Decision: Override 2's PDF half is reverted. The PDF carries buyer name + address
for all customer types. The wire-side data-minimisation (omit customerVatData for
PRIVATE_PERSON, customerAddress per NAV XSD's existing PrivatePerson rules) is
preserved unchanged.

Session 148 made name unconditional. This session (150) makes address unconditional
on the same legal foundation.

This also closes Open question #5 (printed-PDF address for PrivatePerson): the answer
is REQUIRE the address at issuance preflight for PrivatePerson, not merely hint for
the PDF. The partner-form address quartet stays OPTIONAL at save time (operator may
stub a partner); the §169 gate fires at invoice-issuance preflight
(`issue_preflight::customer_address_complete`, fired for Domestic AND PrivatePerson)
and the printed PDF renders the buyer address from the audit-immutable NAV XML
snapshot. nav_xml.rs's `write_customer` is UNCHANGED — `<customerAddress>` is still
emitted only when present, preserving the wire-side data-minimisation.

## Amendment 2026-05-29 (Session 154) — NAV business rule CUSTOMER_DATA_NOT_EXPECTED

Production NAV test endpoint rejected a PrivatePerson invoice (Ervin's invoice 31,
2026-05-29) with business rule CUSTOMER_DATA_NOT_EXPECTED ("Magánszemély vevő adatai
nem adhatók meg."). The rule forbids `<customerName>` and `<customerAddress>` in the
NAV wire format for customerVatStatus = PRIVATE_PERSON.

Sessions 148 and 150 made these elements unconditional in the emit path because §169
of the Hungarian ÁFA Act mandates buyer name + address on the printed invoice. That
work was correct for the PDF (where §169 applies) but accidentally leaked the same
fields into the NAV wire (where they are forbidden for natural-person buyers).

Decision: separate the two surfaces explicitly.
- PDF: ALWAYS emits buyer name + address regardless of customerVatStatus (PR-148/150 preserved).
- NAV wire: SUPPRESSES customerName + customerAddress for PRIVATE_PERSON; emits for
  DOMESTIC + OTHER (this amendment).
- nav-xsd-validator gains a defense-in-depth rule rejecting PrivatePerson bodies
  with name/address before they hit NAV (extends the §5 `ForbiddenChildUnderStatus`
  rule from `<customerVatData>` to `<customerName>` + `<customerAddress>`).

This reaffirms the original ADR-0048 asymmetric stance: §169 governs the printed
invoice document; NAV wire follows NAV's own business rules. It also corrects the
Session-150 amendment's claim that the wire layer "permits absence" of name/address
under PRIVATE_PERSON — the wire layer in fact FORBIDS their presence.
