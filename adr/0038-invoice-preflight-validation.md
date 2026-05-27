# ADR-0038 — `InvoicePreflightError` closed-vocab and typed 400 surface for the `POST /invoices/issue` route (Tier-1 UX #4, PR-69)

- **Status:** Accepted
- **Date:** 2026-05-26
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — operator-correctable
  pre-issuance validation. Extends ADR-0009 §2 (the `Ready`
  typestate's pre-burn gate) by adding a request-shape gate
  BEFORE the issuance pipeline begins, mirroring the existing
  PR-50 supplier-config gate (the `missing_seller_config` typed
  400). The two gates coexist: PR-50 protects supplier identity
  from server-side seller.toml; PR-69 protects the
  operator-typed per-invoice fields (customer + lines + VAT
  rate).
- **Related:**
  - **ADR-0009 §2** — typestate ladder. `Ready` already has a
    server-side XSD gate (ADR-0022). PR-69 adds an
    operator-correctable pre-`Draft` gate so the inline-error
    surface fires BEFORE the audit ledger burns a sequence
    number.
  - **ADR-0022** — `nav-xsd-validator` is the authoritative
    on-the-wire shape check. PR-69 is **strictly weaker** than
    XSD validation by design — it catches the
    operator-correctable subset (empty fields, malformed
    ADÓSZÁM, off-vocab VAT rate) so the operator can fix them
    inline; XSD validation remains the final gate before the
    NAV submit call.
  - **ADR-0037 §3** — `Currency` closed-vocab posture
    (variant-named, ISO 4217 strings via accessor). PR-69 uses
    the same "closed-vocab + named-deferred widening" pattern
    for VAT rates: allowed set `{0, 5, 18, 27}` (Hungarian
    standard rates per Áfa törvény); widening to non-standard
    operator-supplied rates is named-deferred per §"Open
    questions" below.
  - **PR-50** (session-70) — typed `missing_seller_config` 400
    surface for supplier config. PR-69 is the per-invoice-field
    analogue at the same point in the pipeline (route layer,
    before `issue_from_parsed`).

## Context

Tier-1 UX item #4 from the external UX reviewer's analysis
(filed at `project_aberp_ux_roadmap`): *"Pre-flight validation
before NAV submit — surface the same kind of typed errors at
issuance time that NAV would surface at submit time."*

Today (pre-PR-69) the issuance path is asymmetric:

- **Supplier identity** is gated by PR-50's
  `validate_supplier_info` shape check, emerging as the typed
  `missing_seller_config` 400 the SPA renders inline.
- **Customer + lines + currency** are gated only by the
  pre-existing `validate_issue_request` string checks (empty
  lines, empty customer name, empty customer tax number).
  Anything beyond those minimal shape checks — a malformed
  customer ADÓSZÁM, a zero quantity, a non-Hungarian VAT
  percentage — survives the route layer, burns a sequence
  number, lands an `InvoiceDraftCreated` audit entry, gets a
  NAV XML rendered, and either (a) trips the NAV XSD
  validator (operator sees a 500 with the XSD error verbatim)
  or (b) silently passes local validation and gets rejected
  hours later at submit time.

Both surfaces fail too late: (a) bypasses the operator-actionable
inline error the SPA already renders; (b) is the round-trip-to-NAV
delay PR-50 was filed to invert.

The PR-69 cut is a **pre-issuance validation pass** that runs
BEFORE the audit ledger / DB transaction / NAV XML render, with
a typed closed-vocab error surface mirrored on the SPA. Same
shape and same posture as PR-50, expanded to the per-invoice
fields the operator types into the IssueInvoice form.

### Scope discipline — what wire fields exist today

The brief enumerated 15 candidate variants. A naive read would
add all of them. CLAUDE.md rules 7 + 13 push back: validate only
fields that exist on today's wire shape `IssueInvoiceRequest`
(per `apps/aberp/src/serve.rs` and
`apps/aberp-ui/ui/src/lib/api.ts`):

```rust
struct IssueInvoiceRequest {
    customer: CustomerJson { tax_number, name },         // NO address, NO country code
    lines: Vec<LineJson { description, quantity: u32,
                          unit_price: i64, vat_rate_percent: u16 }>,
    currency: Currency,                                   // HUF or EUR (ADR-0037)
    series: Option<String>,
}
```

Variants the brief enumerated that **cannot fire** with this
wire shape (deferred per §"Open questions"):

- `CustomerAddressIncomplete` / `CustomerCountryCodeUnknown` —
  `CustomerJson` has no address today (per PR-44ζ posture);
  validating fields the operator can't supply is rule 7 / rule 13.
- `LineItemCurrencyInconsistent` — `LineJson` has no per-line
  currency; the invoice's `currency` is global per ADR-0037.
- `IssueDateInFuture` — server picks
  `OffsetDateTime::now_utc()` in `issue_from_parsed`; an
  operator-typed future date is impossible from the form.
- `DueDateBeforeIssueDate` — wire shape has no due date
  (today's PDF defaults; PR-44ζ deliberately deferred).
- `NetGrossSumMismatch` — SPA does not declare totals; the
  backend computes them. A mismatch could only fire if the SPA
  started declaring its own computed totals; today there's
  nothing to mismatch against.

These are named-deferred with explicit triggers in §"Open
questions" below — the F12-style discipline for "scope grows
when the wire shape does, not before."

### Variants the brief enumerated that DO fire

- `CustomerNameEmpty` — `customer.name.trim().is_empty()`.
- `CustomerTaxNumberMissing` — `customer.tax_number.trim().is_empty()`.
- `CustomerTaxNumberMalformed` — fails
  `parse_hungarian_tax_number` (reuses PR-50's shape parser).
- `InvoiceLinesEmpty` — already enforced by
  `validate_issue_request` but lifted here into the closed-vocab
  surface so the SPA's per-field renderer can target `lines`
  rather than rendering a string blob.
- `LineItemDescriptionEmpty { line_index }` —
  `line.description.trim().is_empty()`.
- `LineItemQuantityZero { line_index }` — `line.quantity == 0`
  (u32 cannot be negative; non-positive collapses to zero).
- `LineItemUnitPriceNonPositive { line_index, actual }` —
  `line.unit_price <= 0`. An invoice with a `0` or negative
  unit price is not a meaningful business document; a credit-
  note line uses the storno chain (PR-10 + PR-11), not a
  negative line on an issue.
- `LineItemVatRateUnknown { line_index, actual, allowed }` —
  `line.vat_rate_percent` not in the Hungarian Áfa standard-rate
  closed-vocab `{0, 5, 18, 27}`.

Eight variants total. Closed-vocab + deny-default: adding a
ninth requires an explicit enum addition (mirror of
`SupplierConfigError`'s posture per PR-50).

## Decision

**Add a new `apps/aberp/src/issue_preflight.rs` module exposing
a closed-vocab `InvoicePreflightError` enum (eight variants per
§Context) and a pure `validate_invoice_preflight(request:
&IssueInvoiceRequest) -> Vec<InvoicePreflightError>` function
that collects every error in one pass.** The route handler
`handle_issue_invoice` calls it FIRST (before
`supplier_from_seller_toml`, before the issuance pipeline) and
returns a typed `400 Bad Request` with a structured body
the SPA's IssueInvoice form renders inline per field.

### Closed-vocab surface

```rust
pub enum InvoicePreflightError {
    CustomerNameEmpty,
    CustomerTaxNumberMissing,
    CustomerTaxNumberMalformed { actual: String, reason: &'static str },
    InvoiceLinesEmpty,
    LineItemDescriptionEmpty { line_index: usize },
    LineItemQuantityZero { line_index: usize },
    LineItemUnitPriceNonPositive { line_index: usize, actual: i64 },
    LineItemVatRateUnknown { line_index: usize, actual: u16, allowed: &'static [u16] },
}
```

### Validator posture

Pure function, no I/O. Returns `Vec<...>` so ALL errors surface
in one response — the operator fixes everything at once rather
than discovering errors one-by-one on resubmit (same posture as
`setup_seller_info::FieldError` collection per PR-51).

### Wire shape

```json
{
  "error": "invoice_preflight_failed",
  "errors": [
    {
      "kind": "CustomerTaxNumberMalformed",
      "field_path": "customer.taxNumber",
      "message_hu": "Az ÜGYFÉL ADÓSZÁM (`1234567`) hibás formátum (helyes: `xxxxxxxx-y-zz`, pl. `87654321-2-13`)",
      "message_en": "Customer ADÓSZÁM `1234567` is not a valid Hungarian tax number (expected `xxxxxxxx-y-zz`, e.g. `87654321-2-13`)"
    },
    { ... }
  ]
}
```

- `kind` — variant name verbatim (closed-vocab discriminant).
- `field_path` — dotted-path into the wire shape; for line errors
  uses `lines[N].field` indexing so the SPA can route the error
  to the right input.
- `message_hu` + `message_en` — both surfaced; the operator base
  is Hungarian, English is the debugger / English-speaking-
  developer fallback.

### Route handler order

The handler runs validations in this priority order:

1. Bearer check (unchanged).
2. **`validate_invoice_preflight`** (new — PR-69). If
   non-empty, return 400 with the structured body.
3. `validate_issue_request` (legacy — empty-string guard left
   as defence in depth; the preflight variants strictly cover
   its surface).
4. `supplier_from_seller_toml` → `missing_seller_config` 400
   (PR-50, unchanged).
5. Build provider + actor; dispatch to `issue_invoice_request`.

If preflight fails, NOTHING downstream runs: no DB write, no
audit entry, no NAV XML render, no seller.toml read. The
mistake stays at the route layer where the inline-error
renderer can act on it.

### SPA wiring

`IssueInvoice.svelte` parses the typed 400 body via a new
`parseInvoicePreflightErrors(raw)` helper in
`issue-invoice.ts`. Each parsed error renders inline at its
`field_path`'s input (red border + the Hungarian + English
message stacked beneath the input). The Submit button gets a
count badge ("Submit (3 issues)") so the operator sees the
unresolved-count without scrolling.

### Scope discipline

- This PR does NOT change the wire shape of
  `IssueInvoiceRequest`. Adding address fields / due date /
  issue date are separate PRs (per the named-deferred items in
  §"Open questions"), each fires an additional preflight
  variant via the same closed-vocab extension pattern.
- This PR does NOT introduce a new lifecycle state. Preflight
  failure is pre-issuance and never persisted (no audit entry).
- This PR does NOT change the SPA's live-as-you-type behaviour
  (still on-submit only).
- This PR does NOT change `validate_issue_request`'s existing
  surface; the preflight is additive at the route layer.

## Consequences

**What gets easier**

- Operator fixes operator-typeable problems inline at issuance
  time without round-tripping to NAV (the Tier-1 UX #4 goal).
- New per-field validation rules extend the closed-vocab
  surface in one place; the SPA renderer routes them by
  `field_path` without per-rule code changes.
- The "we sent malformed lines and NAV rejected it" failure
  mode is replaced with "we refused to submit because line N's
  VAT rate is not in the Hungarian standard set" — which is
  actionable.
- The audit ledger gets no entries for rejected attempts (a
  rejected preflight = no invoice was ever created), so the
  operator's chain stays clean.

**What gets harder**

- The Hungarian Áfa standard-rate vocab `{0, 5, 18, 27}` is
  hand-maintained. An operator with a legitimate non-standard
  rate (cross-border AAM/TAM, special transitional rates)
  fails preflight and either re-files for the closed-vocab
  widening OR the operator overrides via a future CLI flag
  (named-deferred per §"Open questions"). The trade is small
  (Hungary has stable VAT rates) and the loud-fail naming the
  rejected value is actionable.
- New per-invoice fields require a preflight variant landed in
  the same commit (F12-style discipline). The cost is small —
  the variant is single-line, the test pin is single-line.

**What we lock ourselves into**

- The "preflight is strictly weaker than NAV's XSD" position.
  Preflight catches the operator-correctable subset; NAV-side
  validation remains the source of truth for "is this XML on
  the wire valid." A future PR that wants preflight to mirror
  the full XSD must supersede this ADR.
- The typed 400 body shape (`{error: "invoice_preflight_failed",
  errors: [...]}`) becomes a closed-vocab on the wire — adding
  a new error variant requires the SPA renderer to know about
  it. The pin tests on both sides catch drift per CLAUDE.md
  rule 9.

## Adversarial review

Build-phase. Bar is ≥3 concerns answered or accepted.

- *"You shipped only 8 of the 15 variants the brief enumerated.
  A reviewer reading the brief will see the omission as
  half-finished work."* — Pushed back: the omitted variants
  cannot fire from today's wire shape. CLAUDE.md rules 7 + 13
  are explicit — don't add code for cases that can't happen.
  The handoff document names each deferred variant with its
  trigger (the wire-shape change that would make it reachable).
  Future PRs that widen the wire shape add the corresponding
  variant in the same commit. Accepted.

- *"The Áfa standard-rate vocab `{0, 5, 18, 27}` is more
  opinionated than NAV's XSD, which accepts any non-negative
  decimal. An operator with a legitimately non-standard rate
  hits a false-positive."* — Acknowledged. The trade-off is
  intentional: 99% of real Hungarian invoices use one of the
  four standard rates; an off-by-one typo from 27 → 12 is
  exactly the operator error preflight is filed to catch. The
  loud-fail message names the rejected value AND the allowed
  set, so the operator who legitimately needs a non-standard
  rate has a precise pointer at what to argue for (and the
  pointer becomes the trigger for closed-vocab widening per
  §"Open questions"). Accepted with named-trigger discipline.

- *"You're adding a third validator on the same surface — the
  legacy `validate_issue_request`, the new
  `validate_invoice_preflight`, and the per-call NAV-XSD pass
  via `nav-xsd-validator`. That's three places to maintain."*
  — Acknowledged but rejected as framing. The three surfaces
  have distinct responsibilities: legacy is the defence-in-depth
  for the pre-PR-50 minimal string-shape gate (intentionally
  left as-is per CLAUDE.md rule 3 — surgical changes); PR-69
  is the operator-correctable closed-vocab gate; XSD is the
  on-the-wire authoritative gate. A future PR can fold the
  legacy guard into the new preflight (it's a strict subset);
  named-deferred until a concrete operator-visible need
  surfaces. Accepted.

- *"The Hungarian + English dual-message pattern doubles the
  text payload. Why not pick one language and let the SPA
  translate?"* — Operator base is Hungarian per
  `project_aberp_ui_milestone` posture; English is the
  developer / debug surface. Translation on the SPA side
  duplicates the closed-vocab and lets the messages drift; the
  authoritative messages live with the variant in Rust, the
  SPA renders verbatim. The text overhead is negligible (8
  variants × 2 languages × ~100 bytes ≈ 1.6 KB on a 400 body
  that's only sent on the rare failure path). Accepted.

## Alternatives considered

- **Mirror NAV's full XSD at the route layer.** Rejected —
  duplicates `nav-xsd-validator`'s job and grows the surface to
  match NAV's spec creep instead of operator-correctable
  problems. PR-69 is intentionally weaker than XSD.
- **Field-by-field 400s (return on first error).** Rejected —
  the operator fixes one error, resubmits, hits the next
  error, etc. Single-pass collection per PR-51's posture is
  the operator-respectful shape.
- **Live-as-you-type validation in the SPA.** Rejected —
  duplicates the Rust closed-vocab in TypeScript and lets the
  two drift. On-submit validation is the surgical first cut.
  Live validation is a future thing IF an operator names it as
  a pain point.
- **Persist preflight failures as audit entries.** Rejected —
  a preflight failure means NO invoice was created; there's
  no business event to audit. The audit ledger stays clean.

## Open questions

Each item below is named-deferred with an explicit trigger.
The same closed-vocab extension pattern applies: when the
trigger fires, the corresponding variant lands in the same
commit that widens the wire shape.

- **`CustomerAddressIncomplete { field: AddressField }`.**
  Triggers when `CustomerJson` adds address fields (the PR
  that lifts buyer-address persistence — adjacent to PR-65's
  partners surface but not bundled).
- **`CustomerCountryCodeUnknown { actual: String }`.** Same
  trigger as above; ISO 3166-1 alpha-2 vocab.
- **`LineItemCurrencyInconsistent`.** Triggers when `LineJson`
  adds a per-line `currency` field (cross-currency line is
  ADR-0037 §5 named-deferred).
- **`IssueDateInFuture { issue_date }`.** Triggers when the
  Issue form exposes an operator-supplied issue date (today
  server picks `now_utc()`).
- **`DueDateBeforeIssueDate { issue_date, due_date }`.**
  Triggers when the Issue form exposes a payment due date
  (today PDF defaults).
- **`NetGrossSumMismatch { computed_net, declared_net,
  tolerance_minor }`.** Triggers when the SPA pre-computes and
  declares totals on the wire (today it doesn't).
- **VAT closed-vocab widening to AAM / TAM / TAH special
  categories.** Triggers when an operator names a real cross-
  border invoicing case that needs them. NAV's `vatRateType`
  is a separate XSD construct (non-numeric); widening here
  means the wire shape `LineJson` learns an `Option<String>`
  for the special category, AND the preflight variant
  `LineItemVatCategoryUnknown` lands alongside.
