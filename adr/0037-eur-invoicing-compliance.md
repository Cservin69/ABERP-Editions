# ADR-0037 — EUR-denominated outgoing invoicing: compliance test surface pin (regulatory fields, MNB exchange-rate source, Currency closed-vocab) — extends ADR-0009 §1 ("Currency: HUF only for v1 — trigger: first non-HUF customer signed")

- **Status:** Accepted (legal cleanup 2026-05-23 / session 50)
- **Date:** 2026-05-23
- **Deciders:** Ervin
- **Source-of-legal-confirmation:** Legal citations confirmed
  by Ervin Csengeri, CEO of Aben Consulting, on 2026-05-23
  (session 50). Resolved citations: Áfa tv. §80(1)(g) (HUF
  equivalent must be shown when invoice currency ≠ HUF);
  Áfa tv. §80(2) (applied rate is MNB official mid-rate of
  the fulfillment date, or D-1 if no rate that day —
  weekend / holiday); NAV `Online Számla` XSD fields
  `invoiceData/currencyCode` (ISO 4217),
  `invoiceData/exchangeRate` (decimal, 6 places),
  `invoiceSummary/summaryNormal/invoiceVatAmountHUF`;
  rate precision = 6 decimals per NAV schema; HUF rounding
  = round-half-even per Áfa convention. Placeholders that
  remain open after this cleanup: Áfa tv. §169 (general
  invoice-content list — whether the source name itself
  and the per-VAT-rate net HUF amount are individually
  required on the printed sheet) and Áfa tv. §172
  (storno-currency constraint). F47 partially closed — the
  §80 family + the NAV XSD-field family + the rate precision
  / rounding mode are resolved; F47 remains open for the
  §169 + §172 residue.
- **Class:** Build-phase just-in-time ADR — fires ADR-0009
  §1's named trigger ("first non-HUF customer signed").
  Mid-session-46 scope injection: outgoing invoices must
  support EUR alongside HUF; the EUR/HUF rate must be
  pulled live from the Magyar Nemzeti Bank (MNB, Hungarian
  National Bank); the rate value, rate source, and rate
  date must be printed on the invoice itself per Hungarian
  regulatory requirements. PR-43 is a doc-only ADR that
  pins the **compliance test surface** before any code is
  written, so the subsequent code PRs (α-domain, β-MNB-rates,
  γ-issuance, δ-NAV-submission, ε-UI) have a hard contract
  to build against rather than guessing. Load-bearing deltas:
  §1 (the regulatory-field enumeration on an EUR-denominated
  outgoing invoice — printed-invoice fields + NAV `Online
  Számla` 3.0 schema fields, with `[NEEDS-LEGAL-CHECK]`
  placeholders where citation precision exceeds the
  author's certainty), §2 (the MNB rate source — endpoint,
  date alignment, weekend / holiday fallback), §3 (the
  `Currency` closed-vocab — initial set `{HUF, EUR}`, the
  named trigger for widening), §4 (compliance invariants
  that future code PRs MUST satisfy), §5 (out-of-scope for
  PR-43 — the deferred-items list with named PR-44+
  candidates), §6 (test posture — what a parameterized
  compliance test must check, and what the gate posture
  becomes once the code PRs land). Does **not** supersede
  ADR-0009 §1; extends it by lifting the HUF-only restriction
  on its named trigger. Does **not** introduce a new
  `EventKind` variant (F12 four-edit ritual does NOT fire
  at PR-43 time; whether it fires in PR-44+ is named open
  in §6).
- **Related:**
  - **ADR-0009 §1 — Currency.** The "HUF only for v1"
    clause names "first non-HUF customer signed" as the
    trigger for a separate multi-currency ADR. PR-43 is
    that ADR. ADR-0009 §1 is **extended** (the HUF-only
    restriction is replaced with the Currency closed-vocab
    per §3 below); ADR-0009 §1's command-boundary rejection
    posture is preserved — the boundary now rejects any
    currency code not in the closed vocab.
  - **ADR-0009 §1 — Schema-drift detection.** The vendored
    NAV XSDs already carry the `currencyCode` and
    `exchangeRate` field definitions; the EUR lift does NOT
    require a new XSD pin. The SHA-256-pinned XSD allow-list
    is unchanged.
  - **ADR-0009 §2 — Invoice state machine.** The typestate
    enum (`Draft → Ready → Submitted → AckPending →
    Finalized | Rejected | Abandoned`) is unchanged; EUR
    invoicing rides the same state machine. Currency is
    a field on each typestate's body, not a new typestate.
  - **ADR-0009 §6 — Storno / Amended chain.** The chain
    semantics are currency-agnostic at the state-machine
    level. The currency of a storno or modify-chain child
    invoice MUST match the currency of the chain base
    (per Hungarian regulatory practice — a storno cancels
    its base in the base's denomination); pinned in §4
    below.
  - **ADR-0017 — Design language.** The printed invoice
    is the operator-facing artifact; the regulatory
    field set in §1 below is the printed-invoice
    requirement, NOT the SPA-render requirement. SPA
    render of currency + rate metadata is named-deferred
    per §5 (PR-44ε).
  - **ADR-0021 §"Items deferred to build phase".** The
    "Print rendering path" deferred-item row is the
    natural consumer of §1's printed-invoice field
    list; that ADR remains deferred at PR-43 time per
    its existing trigger ("first PR that produces a
    printed invoice").
  - **ADR-0032 §4 + ADR-0033 + ADR-0034.** The
    audit-evidence chain (`InvoiceSubmissionAttempt` →
    `InvoiceSubmissionResponse` → `InvoiceAckStatus` +
    Layer-2 `InvoiceCheckPerformed` + recover-from-NAV)
    is currency-agnostic. The wire bytes carry the
    currency code and exchange rate as schema-validated
    NAV fields; the audit ledger stores them verbatim
    inside the request/response XML byte slices per
    ADR-0008 §"Storage". No new EventKind variant
    fires at PR-43 time.
  - **ADR-0036 §"Wire-shape preservation".** The
    loopback HTTPS API surfaces (`InvoiceListItem`,
    `InvoiceDetailResponse`) currently expose
    `total_gross` as a HUF integer. PR-44ε's SPA-render
    extension is the consumer of any wire-shape
    additions; PR-43 does NOT extend the wire shape.
- **Source material:** ADR-0009 §1 (currency trigger) +
  Áfa tv. (2007. évi CXXVII. törvény az általános
  forgalmi adóról) §80(1)(g) (HUF equivalent must be
  shown when invoice currency ≠ HUF — confirmed
  2026-05-23) + §80(2) (applied rate = MNB official
  mid-rate of the fulfillment date, or D-1 if no rate
  that day — confirmed 2026-05-23) + §169 + §172
  [NEEDS-LEGAL-CHECK on exact §169 and §172 subsections;
  §80(1)(g) and §80(2) confirmed by Ervin 2026-05-23] +
  NAV Online Számla 3.0 XSD (the vendored copy already
  pinned per ADR-0009 §1 schema-drift detection) + MNB
  exchange-rate publication policy
  (https://www.mnb.hu/arfolyamok — the daily official
  mid-rate) + the operator's scope-injection statement
  during session 46: "Outgoing invoices can be issued
  also in EUR and when I issue it it should pull from
  the national bank. ... The eurhuf rate should be
  pulled from the national bank. ... All invoices must
  be compliant to the regulatory informational
  requirements."

## Context

### What changed mid-session-46

The operator (Ervin) injected scope mid-session-46:
outgoing invoices must support EUR in addition to HUF.
ADR-0009 §1 named this exact scenario as the trigger
for a separate ADR: *"Multi-currency adds a separate
ADR with explicit trigger: first non-HUF customer
signed."* That trigger has now fired.

Two operationally meaningful constraints accompany the
lift:

1. The EUR/HUF exchange rate MUST be pulled live from
   the Magyar Nemzeti Bank (MNB, Hungarian National
   Bank). The MNB's official daily mid-rate is the
   regulatory-acceptable source per Áfa tv. §80(2)
   (confirmed 2026-05-23 — the applied rate is the
   MNB official mid-rate of the fulfillment date, or
   D-1 if no rate is published that day — weekend or
   Hungarian public holiday).
2. The rate value, rate source name ("MNB"), and rate
   date MUST appear on the printed invoice itself.
   The HUF-equivalent gross total MUST also appear on
   the printed invoice when invoice currency ≠ HUF per
   Áfa tv. §80(1)(g) (confirmed 2026-05-23). The
   broader printed-invoice-content list lives under
   Áfa tv. §169 [NEEDS-LEGAL-CHECK on the §169
   subsection that enumerates every printed field —
   §80(1)(g) confirmed; §169 subsection still pending].

### Why a doc-only ADR before any code

The natural temptation is to lift the `Huf` type into
a `Money` sum (`Huf | Eur`) and let the issuance path
follow. That would produce the wrong order of operations:
the code would land before the regulatory contract was
pinned, and the compliance gaps would surface only when
the first EUR invoice reached the printer or NAV's
validator.

PR-43 pins the **compliance test surface** FIRST. The
subsequent code PRs (α through ε per §5 below) build
against a hard contract: the regulatory field set, the
MNB rate source + date-alignment rule, and the
Currency closed-vocab. Each code PR carries a
parameterized test that asserts its slice of the
contract; the contract itself does not drift between
PRs because it lives in this ADR.

This mirrors the posture ADR-0035 took for the bundle
verifier (pin the verifier's invariants before writing
the verifier) and ADR-0036 took for `derive_state` (pin
the mirror invariant before extending the classifier).

### Prerequisite-gate state at PR-43 time

- **Domain types:** `modules/billing/src/domain/money.rs`
  defines `Huf(pub i64)` (whole forints) and an `Eur(pub
  i64)` type (cents, two-decimal precision) that Ervin
  added during the session-46 scope-injection window.
  The `Eur` type exists but is NOT yet wired into any
  invoice typestate (`LineItem.unit_price`,
  `ReadyInvoice::total_gross`, every other
  `total_gross` impl still returns `Huf`). PR-43 does
  NOT modify these types; PR-44α (named in §5) is the
  domain-side lift.
- **Issuance path:** `apps/aberp/src/issue_invoice.rs`,
  `issue_storno.rs`, `issue_modification.rs`,
  `submit_invoice.rs`, `retry_submission.rs`,
  `drain_pending_retries.rs` all operate on the HUF-only
  invoice surface. None reference `Eur`. PR-43 does
  NOT modify any of these; PR-44γ is the issuance-side
  lift.
- **NAV-submission body:** the XML builder produces the
  v3.0 `InvoiceData` body with `currencyCode = "HUF"`
  hard-coded (or equivalently absent, defaulting to
  HUF). PR-43 does NOT modify the builder; PR-44δ is
  the submission-body lift.
- **MNB-rate fetch:** no crate exists. PR-43 does NOT
  create one; PR-44β is the rate-source lift.
- **SPA render:** `apps/aberp-ui/ui/src/lib/api.ts`
  and `InvoiceDetail.svelte` render `total_gross` as
  a HUF integer with the `HUF` suffix. PR-43 does NOT
  modify these; PR-44ε is the SPA-render lift.
- **NAV Online Számla 3.0 XSD:** vendored at the
  SHA-256-pinned patch level per ADR-0009 §1. The
  schema already carries `currencyCode` (ISO 4217)
  and `exchangeRate` (decimal) on `InvoiceHead` and
  HUF-equivalent fields (`vatRateNetAmountHUF`,
  `vatRateVatAmountHUF`) on the VAT summary. **No XSD
  re-pin is required** for the EUR lift; the
  schema-drift detection posture remains intact.

### Surfaced conflicts (CLAUDE.md rule 7)

**Conflict 1 — rate date: supply-fulfillment date vs.
day-before vs. issuance date.** Áfa tv. §80 names the
rate prevailing at the moment the VAT obligation
arises (typically the supply-fulfillment date —
`teljesítés napja`), with the most-recent published
rate used when the supply-fulfillment date is a
non-publication day (weekend / Hungarian public
holiday). **Reading A:** use the rate of the
supply-fulfillment date (or the most-recent prior
publication date if that day has no rate).
**Reading B:** use the rate of the issuance date.
**Reading C:** use the rate of the day BEFORE the
supply-fulfillment date. **Decision below picks
Reading A** — the §80(2) rule (confirmed 2026-05-23:
MNB official mid-rate of the fulfillment date, or D-1
if no rate that day) is canonical for VAT-base
calculation; the issuance date and the
flat-day-before reading are not regulatory. The §80(2)
text covers Reading A's walked-back-fallback for
non-publication days; the legal-review pass at
session 50 confirmed this reading.

**Conflict 2 — MNB endpoint: SOAP `MNBArfolyamServiceSoap`
vs. the JSON endpoint.** MNB publishes daily exchange
rates via two surfaces: the historic SOAP service
(`http://www.mnb.hu/arfolyamok.asmx?WSDL`,
`GetCurrentExchangeRates` / `GetExchangeRates`
operations) and a more recent JSON API. **Reading A:**
the SOAP service is the official, regulatorily
referenced endpoint and is the safer pick.
**Reading B:** the JSON API is simpler to integrate
and reflects MNB's modern publication posture.
**Decision below picks Reading A** — the SOAP service
is the long-standing, documented surface; the JSON
API's availability and contract stability are
properly verified at PR-44β implementation time.
A switch to Reading B (or addition of B as a fallback)
remains a PR-44β implementation decision and does NOT
require a new ADR unless the regulatory-source
identifier on the printed invoice changes from "MNB"
to anything else.

**Conflict 3 — Currency closed-vocab vs. open ISO 4217
acceptance.** **Reading A:** define `Currency` as a
typed Rust enum with the initial set `{HUF, EUR}`,
widen via additive enum variant + ADR.
**Reading B:** accept any ISO 4217 currency code as a
string and rely on the NAV XSD's `currencyCode`
validation. **Decision below picks Reading A** — the
typestate posture (ADR-0009 §2) and the wire-only
typed-enum posture (A119, A127, A130 from the 31→46
window) make a closed vocab the correct default; the
domain types stay exhaustive over the supported
currencies; adding a third currency is a one-line
enum-variant addition gated on a named trigger. NAV
XSD-level validation is the **outer** boundary;
the **inner** boundary (ABERP's domain) refuses
anything not in the enum.

**Conflict 4 — chain-child currency: must match base
vs. can differ.** **Reading A:** a storno or
modify-chain child invoice MUST be denominated in
the same currency as the chain base (Hungarian
regulatory practice — the cancellation cancels the
base in the base's denomination). **Reading B:**
allow the chain child to use a different currency
with an explicit exchange-rate reconciliation.
**Decision below picks Reading A** — same-currency
chain children match the regulatory practice and
keep the chain-walker (ADR-0036 §4) currency-agnostic;
any future requirement for cross-currency chain
children lands as a separate ADR. PR-44γ's issuance
extension MUST refuse a chain-child issuance whose
currency differs from the base's currency, with a
loud-fail error.

## Decision

### 1. Regulatory field set on an EUR-denominated outgoing invoice

A printed outgoing invoice denominated in EUR (or any
non-HUF currency the closed vocab admits per §3 below)
MUST carry every one of the following fields. The list
is the printed-invoice contract; the NAV `Online
Számla` 3.0 wire-body contract is in §1.b below.

#### 1.a Printed-invoice fields (Áfa tv. §169 informational requirement)

The printed invoice MUST show, in addition to every
field a HUF invoice already shows:

- **Invoice currency code.** ISO 4217 three-letter
  code (`EUR`). Printed in the totals area; conventional
  placement is adjacent to each amount (e.g., `1 234.56
  EUR`).
- **Exchange rate value.** The MNB-published EUR/HUF
  mid-rate, printed at the precision MNB publishes
  (currently 2 decimal places, e.g., `405.23`). The
  printed format MUST disambiguate the direction (per
  Hungarian convention: HUF per 1 EUR, e.g., `1 EUR =
  405.23 HUF`).
- **Exchange-rate source name.** The literal string
  `MNB` (or the operator-visible expansion `Magyar
  Nemzeti Bank`). The source identifier is part of the
  regulatory record per Áfa tv. §80 [NEEDS-LEGAL-CHECK
  on whether the source name itself is required on the
  printed invoice or only in the operator's records;
  the safer posture — print it — is the decision].
- **Exchange-rate date.** The publication date of the
  MNB rate that was applied (ISO-8601 `YYYY-MM-DD`).
  Per §2 below, this is the publication date of the
  rate valid on the supply-fulfillment date (or the
  most-recent prior publication date if that day has
  no rate).
- **HUF-equivalent gross total.** The full invoice
  gross total expressed in HUF, computed at the
  applied exchange rate. Printed adjacent to the
  EUR gross total, conventionally labelled `Összesen
  HUF-ban` or equivalent.
- **HUF-denominated VAT line(s).** Each VAT rate's
  VAT amount expressed in HUF (the regulatory
  requirement is that NAV's per-rate VAT reporting
  is in HUF; the printed invoice surfaces the same
  HUF figure so the operator and the customer can
  reconcile against NAV's record). The HUF-denominated
  net amount per VAT rate is OPTIONAL on the print
  per §169 [NEEDS-LEGAL-CHECK]; the printed invoice
  MAY include it for parity with the wire body's
  field set.

#### 1.b NAV Online Számla 3.0 wire-body fields

The vendored v3.0 XSD already defines these fields;
PR-44δ's submission-body extension populates them
when `currencyCode != HUF`:

- **`invoiceMain/invoice/invoiceHead/invoiceDetail/currencyCode`** —
  ISO 4217 currency code on the invoice (e.g., `EUR`).
- **`invoiceMain/invoice/invoiceHead/invoiceDetail/exchangeRate`** —
  the decimal exchange rate value. The XSD currently
  requires positive decimal; the value populated MUST
  match the printed-invoice value per §1.a.
- **`invoiceMain/invoice/invoiceSummary/summaryNormal/summaryByVatRate/[vatRateNetAmount, vatRateVatAmount]`** —
  per-VAT-rate net and VAT amounts in invoice currency
  (EUR).
- **`invoiceMain/invoice/invoiceSummary/summaryNormal/summaryByVatRate/[vatRateNetAmountHUF, vatRateVatAmountHUF]`** —
  per-VAT-rate net and VAT amounts in HUF (the
  regulatory HUF-equivalent). MUST be populated when
  `currencyCode != HUF`.
- **`invoiceMain/invoice/invoiceSummary/summaryNormal/invoiceNetAmount`** /
  **`invoiceVatAmount`** — invoice-level totals in
  invoice currency.
- **`invoiceMain/invoice/invoiceSummary/summaryNormal/invoiceNetAmountHUF`** /
  **`invoiceVatAmountHUF`** — invoice-level totals in
  HUF. MUST be populated when `currencyCode != HUF`.

Confirmed NAV Online Számla XSD field paths
(2026-05-23): `invoiceData/currencyCode` (ISO 4217
three-letter code), `invoiceData/exchangeRate`
(decimal, 6 decimal places), and
`invoiceSummary/summaryNormal/invoiceVatAmountHUF`
(decimal, the regulatory HUF-equivalent VAT total).
The fuller `invoiceMain/invoice/...` paths above are
the in-body wrapper context; PR-44δ's golden-XML test
pins both the wrapper context and the confirmed
field-leaf names against the vendored XSD at
implementation time.

#### 1.c Rounding and HUF-conversion precision

- **HUF-conversion rule.** Per-VAT-rate HUF amounts
  are computed by converting the EUR amount at the
  applied exchange rate, then rounding to the whole
  forint (HUF has no sub-unit per ADR-0009 §1 /
  `Huf(pub i64)`). The rounding mode is
  **round-half-even** (banker's rounding) per Áfa
  convention (confirmed 2026-05-23). Ties round to
  the even forint (`123.5 → 124`, `124.5 → 124`); this
  is the Áfa-compliant rule and supersedes the
  pre-cleanup half-up posture.
- **Exchange-rate value precision.** The exchange rate
  is stored and serialized to the NAV-submitted XML at
  **6 decimal places** per the NAV `Online Számla` XSD
  (confirmed 2026-05-23). The printed-invoice display
  per §1.a MAY show fewer decimals (MNB publishes the
  EUR/HUF mid-rate at lower precision); the wire body
  carries the 6-decimal form regardless of the printed
  form.
- **Per-line vs. per-VAT-rate posture.** The HUF
  conversion happens at the per-VAT-rate summary
  level, NOT per-line. Per-line totals stay in EUR
  on both the printed invoice and the wire body;
  only the VAT summary section carries HUF
  equivalents. This matches the NAV v3.0 XSD's
  field placement.
- **Invoice-level total HUF amount.** Computed as
  the sum of the per-VAT-rate HUF amounts, NOT by
  converting the EUR invoice total directly. This
  preserves the per-VAT-rate breakdown's internal
  consistency.

### 2. MNB exchange-rate source

#### 2.a Endpoint

- **Primary endpoint:** the MNB SOAP service at
  `http://www.mnb.hu/arfolyamok.asmx?WSDL`, exposing
  the `MNBArfolyamServiceSoap` interface. The
  operations consumed by PR-44β:
  - `GetCurrentExchangeRates` — returns the current
    day's rates; used as a sanity check on cache
    freshness.
  - `GetExchangeRates(startDate, endDate, currencies)` —
    returns the rate for a specific historical date;
    the canonical query for the rate-on-supply-date
    lookup per §2.b below.
- **Source identifier on the printed invoice:** the
  literal string `MNB`. This is the value PR-44ε
  renders verbatim.
- **Fallback endpoint:** none in PR-44β scope. If MNB
  is unreachable at issue time, the issuance path
  MUST refuse (loud-fail) rather than fall back to a
  cached-but-stale rate or a non-MNB rate source.
  Operator-visible error: `MNB rate unavailable for
  <date>; refuse to issue EUR invoice`. This is
  pinned in §4 below as a compliance invariant.

#### 2.b Date alignment

- **Primary rule (Áfa tv. §80(2), confirmed 2026-05-23):**
  the applied exchange rate is the MNB official mid-rate
  published for the **supply-fulfillment date**
  (`teljesítés napja`). When no rate is published that
  day (weekend, Hungarian public holiday), the rate of
  D-1 is used; if D-1 also has no published rate (long
  weekend, multi-day holiday window), the walk-back
  continues to the most-recent prior publication date.
- **Non-publication-day fallback:** as above — D-1 is the
  primary fallback per §80(2); the walk-back extends as
  needed for multi-day non-publication windows. The
  `Exchange-rate date` printed per §1.a is the
  publication date that was actually consulted, NOT
  the supply-fulfillment date.
- **Pre-PR-43 invoices:** all existing invoices in
  the audit ledger are HUF-denominated (the `Huf`
  type was the only currency available pre-PR-43);
  retroactive backfill is not in scope.
- **Caching posture:** PR-44β MAY cache MNB responses
  per (currency, date) tuple — the rate is deterministic
  for a (currency, date) tuple. The cache MUST be
  invalidated for the current day if the cached value
  predates an MNB re-publication (rare but possible
  for prior-day corrections); the simpler posture is
  to re-query MNB on every issuance and let the
  operational cost surface only if it becomes a
  problem. PR-44β implementation pick is named open.

#### 2.c What the audit ledger records

- **In the wire bytes:** the `currencyCode` and
  `exchangeRate` fields populate the NAV-submitted
  XML body per §1.b. The audit ledger stores those
  XML bytes verbatim inside `InvoiceSubmissionAttempt`
  /  `InvoiceSubmissionResponse` payloads per ADR-0008
  §"Storage"; the exchange rate is recoverable from
  the bundle without a separate audit-event entry.
- **No new EventKind variant.** PR-43's pin AND PR-44β
  through PR-44ε's lifts SHOULD compose the existing
  audit surface; the exchange rate is wire-body
  content, the rate fetch is a deterministic
  pre-issuance step. Whether PR-44β surfaces a
  separate `ExchangeRateFetched` audit event (for
  cache-hit-vs-cache-miss observability) is named
  open in §5 below; the default is **no** — the
  ledger already records the applied rate inside the
  wire bytes, and a separate fetch event is
  speculative per CLAUDE.md rule 2.

### 3. `Currency` closed-vocab

- **Type:** typed Rust enum `Currency`, derived with
  `Debug + Clone + Copy + PartialEq + Eq + Hash`
  (mirrors `Huf`'s derive set per
  `modules/billing/src/domain/money.rs`).
- **Initial variant set:** `{ Huf, Eur }` (two
  variants — exhaustive over the supported
  currencies at PR-43 time).
- **Variant-name posture:** the enum variant names
  match the existing money-type names (`Huf`, `Eur`)
  rather than the ISO 4217 codes (`HUF`, `EUR`). The
  ISO 4217 string code is surfaced via a
  `Currency::iso_code(&self) -> &'static str` accessor
  for wire / printed surface use.
- **Widening trigger:** named additively per ADR-0009
  §1's posture — a new variant lands when the
  operator signs a customer needing that currency.
  Likely future variants (named, not pinned): `Chf`
  (Swiss franc — common in Hungarian B2B), `Usd`
  (US dollar). Each addition is a one-line enum
  variant change + a one-row test addition + a
  one-line ADR-0037-amendment note.
- **Command-boundary refusal posture (preserves
  ADR-0009 §1):** the issuance path refuses any
  currency value not in the closed vocab. PR-44α's
  domain-side lift converts the existing HUF-only
  command surface into a currency-aware command
  surface; the rejection is now "currency not in
  the closed vocab" rather than "currency != HUF".
- **No string-typed currency on the domain.** ISO
  4217 strings appear only on the wire boundary
  (NAV XML body, printed invoice text, SPA render)
  via the `iso_code` accessor; the domain never
  carries a `String` currency code.

### 4. Compliance invariants the code PRs MUST satisfy

This is the **hard contract** subsequent code PRs build
against. Each invariant has a code-PR owner and a test
posture (the test is gated when its owning code PR
lands; the invariant itself is asserted by this ADR
and propagates via the test).

| # | Invariant | Owner PR | Test posture |
|---|---|---|---|
| C1 | Issuance refuses an EUR invoice with a missing exchange rate (the `Currency::Eur` branch MUST resolve to a rate before the `ReadyInvoice` typestate transition). | PR-44α (domain) | Unit test on the issuance command — `Currency::Eur` + missing rate → loud-fail `MissingExchangeRate` error. |
| C2 | Issuance refuses an EUR invoice when the MNB rate is unavailable (MNB unreachable, timeout, malformed response). No silent fallback. | PR-44β (mnb-rates) | Integration test with an MNB stub returning a transport error → loud-fail `MnbRateUnavailable` error; no rate cache fallback. |
| C3 | The applied exchange rate's publication date matches the supply-fulfillment date OR the most-recent prior publication date (§2.b rule). | PR-44β | Parameterized test over (supply-date, MNB-publication-calendar) pairs → asserts the applied publication date is the largest publication date `≤` supply-date. |
| C4 | The NAV-submitted XML body populates `currencyCode` and `exchangeRate` per §1.b; per-VAT-rate HUF amounts populate `vatRateNetAmountHUF` and `vatRateVatAmountHUF`. | PR-44δ (NAV submission) | Golden-XML test on the builder — EUR invoice → XML body contains `currencyCode=EUR` + numerically correct `exchangeRate` + per-rate HUF equivalents that sum to the invoice-level HUF total. |
| C5 | The HUF-equivalent total on the wire body equals the sum of the per-VAT-rate HUF amounts (§1.c per-VAT-rate posture, NOT direct conversion of the EUR invoice total). | PR-44δ | Property-style test — generate N EUR invoices with mixed VAT rates → assert `sum(vatRateNetAmountHUF + vatRateVatAmountHUF)` equals `invoiceNetAmountHUF + invoiceVatAmountHUF`. |
| C6 | A storno-chain or modify-chain child invoice MUST be denominated in the same currency as the chain base (§Surfaced conflict 4 Reading A). | PR-44γ (issuance) | Unit test on the chain-issuance commands — base in HUF + child requested in EUR → loud-fail `ChainCurrencyMismatch` error; same for base in EUR + child requested in HUF. |
| C7 | The printed-invoice render carries every §1.a field. | PR-44ε (UI) + the future print-rendering ADR (deferred per ADR-0021) | SPA component test for §1.a (SPA-visible portion) + print-render test once the print-rendering PR lands. |
| C8 | The `Currency` enum is closed; the command boundary refuses any currency not in the closed vocab (§3 refusal posture). | PR-44α | Unit test — issuance command with `currency = "CHF"` (string-typed on the API boundary) → loud-fail `UnsupportedCurrency` error. |
| C9 | The exchange rate's source identifier on the printed invoice is the literal string `MNB`. | PR-44ε | SPA / print-render test asserting the rendered source name is exactly `MNB` (or the operator-visible expansion `Magyar Nemzeti Bank`). |
| C10 | No retroactive rewrite of HUF-only invoices into the new `Currency::Huf` shape changes the audit-ledger byte contents. The migration is a domain-type rename only; the wire bytes for existing HUF invoices are byte-identical pre/post PR-44α. | PR-44α | Differential test — replay a representative sample of pre-PR-44α HUF invoices through the post-PR-44α builder; assert the produced XML bytes are byte-identical to the on-disk audit-ledger bytes. |
| C11 | The NAV-submitted `exchangeRate` carries six decimal places per the `Online Számla` XSD; per-VAT-rate HUF amounts are rounded with round-half-even (banker's rounding) per Áfa convention. | PR-44β (rate-value serialization) + PR-44δ (HUF rounding application) | Property-style test — pin `exchangeRate` serialization at 6-decimal precision (`405.230000`, NOT `405.23`); unit tests pinning the half-even tie-break case for HUF amounts (`123.5 HUF → 124`, `124.5 HUF → 124`); both tests assert against the confirmed §80(2) + Áfa convention combination. |

These invariants compose. C1+C2+C3 pin the rate-fetch
slice; C4+C5 pin the wire-body slice; C6 pins the
chain-currency slice; C7+C9 pin the print-render
slice; C8 pins the closed-vocab refusal slice; C10
pins the migration safety slice; **C11 pins the
rate-precision + HUF-rounding-mode slice (added at
the 2026-05-23 legal cleanup)**.

### 5. Out-of-scope for PR-43 (deferred-items list with named PR-44+ candidates)

PR-43 is a pure-ADR PR. The following lifts are
**explicitly out of scope** and named here so the
deferred-items table in the session-47 close handoff
(and ADR-0021's table) carries them as concrete
candidates with named triggers:

| PR | Scope | Trigger |
|---|---|---|
| **PR-44α** — `Money` shape generalization | Lift `Huf` and `Eur` into a unified `Currency`-aware money type (or an enum-of-money sum); update every typestate's `unit_price` and `total_gross` and the `LineItem` shape to carry the currency. ~300-500 LoC across `modules/billing/src/domain/`. F12 ritual NOT expected to fire. | C1+C8+C10 invariants from §4. First PR after PR-43 lands. |
| **PR-44β** — `mnb-rates` crate | New workspace crate `crates/mnb-rates` exposing a `fetch_rate(currency, date) -> Result<ExchangeRate, MnbRateError>` API. SOAP client over the MNB endpoint per §2.a. ~200-400 LoC. F12 ritual NOT expected to fire (no audit event by default — see §2.c open question). | C2+C3 invariants. Lands after PR-44α (depends on `Currency` enum). |
| **PR-44γ** — Issuance-path EUR extension | Wire the `Currency` enum into `issue_invoice.rs`, `issue_storno.rs`, `issue_modification.rs`, `submit_invoice.rs`, `retry_submission.rs`, `drain_pending_retries.rs`. Refuse mismatched-currency chain children per C6. ~200-400 LoC across `apps/aberp/src/`. F12 ritual NOT expected to fire. | C6 invariant. Lands after PR-44α + PR-44β. |
| **PR-44δ** — NAV-submission body EUR extension | Extend the v3.0 XML builder to populate `currencyCode`, `exchangeRate`, `vatRateNetAmountHUF`, `vatRateVatAmountHUF`, `invoiceNetAmountHUF`, `invoiceVatAmountHUF` per §1.b. ~150-300 LoC in the builder. F12 ritual NOT expected to fire. | C4+C5 invariants. Lands after PR-44α + PR-44β. |
| **PR-44ε** — SPA render EUR extension | Surface currency + exchange-rate metadata on `InvoiceListItem` + `InvoiceDetailResponse` wire shape; render the §1.a fields on the SPA invoice-detail view. ~100-200 LoC across `apps/aberp-ui/ui/`. F12 ritual NOT expected to fire. Print-render is a separate future PR per ADR-0021's "Print rendering path" deferred row. | C7+C9 invariants (SPA-visible portion). Lands after PR-44δ. |

Additional explicitly named-deferred items, with
triggers, that PR-44+ candidates should NOT speculatively
include (CLAUDE.md rule 2):

| Item | Trigger |
|---|---|
| `ExchangeRateFetched` audit event (per (currency, date) cache observability) | First operator survey reporting that rate-fetch latency / cache-behaviour is not diagnosable from the wire bytes alone. The default per §2.c is NO new event. |
| MNB JSON endpoint as fallback / replacement for the SOAP endpoint | First operational case where the SOAP service is unreliable AND the JSON service is verified stable. |
| ~~Banker's-rounding alternative to half-up HUF conversion~~ | **RESOLVED 2026-05-23 (session 50): round-half-even per Áfa convention is the active rule per §1.c; half-up was the pre-cleanup posture and is superseded.** No deferred row remains. |
| Cross-currency chain children (storno of HUF base in EUR or vice versa) | Operator request OR regulatory-practice change. PR-43 refuses per C6. |
| Third (and Nth) currency variant on the `Currency` enum (`Chf`, `Usd`, etc.) | Operator signs a customer needing that currency per ADR-0009 §1's posture inherited here. |
| Print-rendering path for the EUR invoice (the §1.a fields on the printed sheet) | The existing ADR-0021 "Print rendering path" deferred row — same trigger ("first PR that produces a printed invoice"). |
| Retroactive backfill of pre-PR-44α HUF invoices into the new `Currency::Huf` shape | C10 invariant means no backfill is needed; the type rename is sufficient. |
| Per-line currency mismatch within one invoice (a multi-currency invoice) | NOT in scope per Hungarian regulatory practice — an invoice is denominated in one currency. PR-44α MUST refuse this configuration with a loud-fail. |

### 6. Test posture — what compliance tests must check AND what the gate posture becomes

PR-43 itself does NOT add tests (it is doc-only). The
test surface lands in PR-44α through PR-44ε per the
"Test posture" column in §4. The session-47 close
handoff carries the gate baseline UNCHANGED from
session 46.

**Once all of PR-44α through PR-44ε have landed**, the
compliance test surface MUST include:

1. **A parameterized EUR-issuance test** that walks
   the full path: command → `Currency::Eur` → MNB rate
   fetch → `ReadyInvoice` → submission body → wire
   bytes → audit-ledger entry. Asserts every C1-C10
   invariant on each parameterized row.
2. **A regression test that EVERY EXISTING HUF invoice
   surface remains byte-identical post-PR-44α** (C10
   invariant). Differential replay against the
   pre-PR-44α audit-ledger bytes.
3. **A negative test per refused-currency case** — the
   command boundary refuses with the right error kind
   for each of: unknown ISO code, malformed code, empty
   currency field, chain-currency mismatch (per C6),
   intra-invoice multi-currency (per §5's
   per-line-currency-mismatch deferred row).
4. **A property-style test on the HUF-conversion
   arithmetic** that pins the half-up rounding rule
   per §1.c and the per-VAT-rate summation rule per
   C5.

PR-43's gate baseline is **UNCHANGED** from session 46:

| Layer | Count |
|---|---|
| `cargo check --workspace` | clean (no code touched) |
| `cargo test --workspace` | 470 / 0 / 1 (UNCHANGED) |
| `npm run check` | 131 files, 0 errors, 0 warnings (UNCHANGED) |
| `npm test` | 28 passed in 2 files (UNCHANGED) |

The next gate move is in PR-44α and onwards; PR-43
itself touches only `adr/` and `_handoffs/`.

## Consequences

**What gets easier.**

- **The code PRs have a hard contract.** PR-44α
  through PR-44ε build against §1 + §2 + §3 + §4
  rather than guessing the regulatory surface mid-
  implementation. Code reviewers can read this ADR
  alongside each code PR and verify the C1-C10
  invariants are upheld; the regulatory contract does
  not drift between PRs.
- **The Currency closed-vocab keeps the domain
  exhaustive.** A new currency is a one-line enum
  addition; pattern matches over `Currency` are
  exhaustive by Rust's normal mechanism; the wire
  boundary's ISO 4217 string lives in one place
  (the `iso_code` accessor).
- **The MNB-rate fetch slice is isolated.** A separate
  `crates/mnb-rates` crate (PR-44β) means the rest of
  the codebase consumes a typed `ExchangeRate` value;
  the SOAP / JSON / fallback details are
  implementation choices, not API choices.
- **No retroactive bytes change.** C10 means the
  pre-PR-44α HUF invoices' audit-ledger entries are
  byte-identical pre/post the lift; the bundle
  verifier (ADR-0035) keeps passing on every existing
  bundle without modification.
- **The Currency enum surfaces in tests as
  exhaustively.** Adding a future variant (`Chf`,
  `Usd`) surfaces every test that pattern-matched
  `Currency` non-exhaustively as a compile error —
  this is the same rebuild-on-add posture A127
  established for the disjoint-concept-domain pattern.

**What gets harder.**

- **The §1.a printed-invoice field set is a hard
  contract.** Future print-render work (the ADR-0021
  "Print rendering path" deferred row) MUST surface
  every field in §1.a; partial coverage is
  non-compliant. PR-44ε ships the SPA-visible portion;
  the printed-invoice portion lands with the
  print-rendering PR.
- **MNB unavailability becomes an issuance-blocker
  for EUR invoices.** Per C2 there is no fallback rate
  source. If MNB is down at issuance time, the
  operator cannot issue an EUR invoice that day. The
  loud-fail posture is intentional — silent rate
  fallback to a stale value would produce a
  regulatorily incorrect invoice. The operator-visible
  error names the date and the failure.
- **PR-44β depends on a third-party network surface.**
  The MNB SOAP service is a publication endpoint; its
  availability is outside ABERP's control. The
  loud-fail posture above contains the blast radius
  (no incorrect issuance) but means EUR-customer-facing
  operations have a new external dependency. The
  cadence of MNB outages is operationally low (the
  service has run for decades) but the surface is
  named.
- **The `Currency` enum widens are gated on legal
  and operational review, not just the trigger
  firing.** Each new variant lands with an ADR-0037
  amendment naming the source-of-rate decision (likely
  still MNB for CHF / USD since MNB publishes
  cross-rates for major currencies) and any
  variant-specific regulatory nuance.
- **Chain-walker invariants extend to currency
  matching.** PR-44γ's chain-issuance refusal per C6
  adds a precondition check on the chain-base
  invoice's currency; the chain-walker (ADR-0036 §4)
  is currency-agnostic at the read side, but the
  issuance side is not.

**What we lock ourselves into.**

- **MNB as the rate source.** Switching to a different
  rate publisher (e.g., the European Central Bank's
  ECB reference rates) is an ADR-amendment scope.
  Until then, the printed-invoice source identifier
  is `MNB` and the wire body carries the MNB
  publication date.
- **Reading A on §Surfaced conflict 1 (rate date =
  supply-fulfillment date).** Switching to Reading B
  (issuance date) or Reading C (day-before) is an
  ADR-amendment scope. Until then, the
  publication-date-walk-back rule in §2.b is canonical.
- **Reading A on §Surfaced conflict 2 (SOAP primary).**
  Switching to the JSON endpoint primary is a PR-44β
  implementation decision; switching to a non-MNB
  source identifier is an ADR-amendment scope.
- **Reading A on §Surfaced conflict 3 (closed
  Currency vocab).** Switching to open ISO 4217
  acceptance is an ADR-amendment scope. Until then,
  every new currency lands with a named PR.
- **Reading A on §Surfaced conflict 4 (chain children
  match base currency).** Cross-currency chain
  children require an ADR-amendment scope.

## Adversarial review

> The ADR template names "at least three adversarial
> concerns." This list deliberately runs longer to
> match the load-bearing nature of a multi-PR contract.

1. **"You marked legal citations `[NEEDS-LEGAL-CHECK]`
   without resolving them. A regulatorily incorrect
   invoice is a fine."** PARTIALLY RESOLVED at the
   2026-05-23 legal cleanup (session 50): §80(1)(g)
   + §80(2) + the NAV XSD field-path family + the
   rate precision (6 decimals) + the HUF rounding
   mode (round-half-even per Áfa convention) are now
   confirmed citations. The §169 invoice-content list
   subsection and the §172 storno-currency
   subsection remain `[NEEDS-LEGAL-CHECK]` because
   Ervin's source walk did not surface a subsection
   precise enough to cite. The C1-C11 compliance
   invariants are independent of the §169 + §172
   residues — they pin the behaviour even if those
   §-numbers resolve to slightly different sections.
   Mitigation unchanged: moving any remaining
   `[NEEDS-LEGAL-CHECK]` line from "needs check" to
   "resolved" requires an explicit ADR amendment,
   NOT a comment in a code PR.
2. **"You picked MNB SOAP without verifying it's
   actually online and stable in 2026. MNB might have
   deprecated it."** Accepted. PR-44β implementation
   verifies the endpoint is live before it lands; if
   the SOAP service is deprecated by the time PR-44β
   starts, this ADR is amended (Reading B from
   §Surfaced conflict 2 promotes to primary). The
   printed-invoice source identifier `MNB` is
   endpoint-independent — both SOAP and JSON endpoints
   publish the same MNB-authoritative rates.
3. **"The half-up rounding rule in §1.c is wrong;
   Hungarian regulation actually requires banker's
   rounding."** Confirmed at the 2026-05-23 legal
   cleanup. §1.c now pins round-half-even (banker's
   rounding) per Áfa convention; the pre-cleanup
   half-up posture was wrong. §5's "Banker's-rounding
   alternative" deferred row is removed (it is now
   the active rule, not the alternative). PR-44α's
   HUF-conversion test MUST pin round-half-even
   (the half-up pin the pre-cleanup ADR named is now
   superseded; if PR-44α had already landed with
   half-up, it would be a test re-pin + a one-line
   rounding-mode change at the conversion call site).
4. **"C10 (byte-identical HUF audit ledger pre/post
   PR-44α) is impossible if the type rename produces
   different serialization output."** Defensible.
   The audit-ledger bytes are the wire-body XML +
   the typed audit payloads, neither of which carries
   the Rust type name. The `Huf` → `Currency::Huf`
   rename is a domain-side change; serialization
   downstream produces the same string `"HUF"` and
   the same integer-forint amount. The C10 test
   IS the assertion that this defensible expectation
   holds; if it fails at PR-44α implementation time,
   the lift is not green and the ADR is amended.
5. **"You named PR-44α through PR-44ε but did not
   pin their ORDER. A green PR-44γ might land before
   PR-44β does."** §5 names the dependencies in the
   trigger column: PR-44α first (domain), then
   PR-44β (rates), then PR-44γ + PR-44δ in either
   order (both depend only on α + β), then PR-44ε
   (depends on δ). The session handoffs will keep
   the order pinned across sessions.
6. **"A storno-chain child denominated in the same
   EUR as its base would still need a fresh MNB rate
   for ITS issuance date, not the base's. The §2.b
   rule applies per invoice, not per chain."**
   Correct, and pinned implicitly by §2.b ("the
   supply-fulfillment date" — every invoice has its
   own). PR-44γ's chain issuance MUST fetch a fresh
   rate per the child's supply-fulfillment date, not
   reuse the base's rate. C6 only constrains the
   currency code, not the rate value. The compliance
   test in §6.1 walks this per-invoice rate fetch
   for chain children explicitly.
7. **"NAV's v3.0 XSD may have evolved between the
   vendored patch level and 2026; the §1.b XPath
   list may be slightly wrong."** Possible. The
   schema-drift detection per ADR-0009 §1 SHA-256-
   pins the XSDs; if NAV publishes a new patch
   level, the operator-visible loud-fail surfaces
   it BEFORE PR-44δ's XPath-dependent code attempts
   submission. The §1.b XPath list is treated as
   the canonical contract at the vendored patch
   level AT PR-44δ implementation time; if NAV's
   then-current XSD differs from the vendored copy,
   the PR-44δ test pins against the vendored copy
   (the source of truth per ADR-0009 §1) and a
   separate XSD-re-pin PR handles the upgrade.
8. **"You allow a chain-child storno of an EUR base
   to use a DIFFERENT MNB rate than the base did
   (C6 only constrains currency, not rate). The
   storno's HUF-equivalent may not equal the base's
   HUF-equivalent."** Accepted. Hungarian regulatory
   practice treats a storno as a fresh invoice for
   the cancellation transaction (with its own
   issuance date and its own supply-fulfillment date
   per the rules under which the storno is being
   issued); the rate alignment is per-invoice, NOT
   chain-aligned. The HUF-equivalent on a storno
   chain child is computed at the storno's rate, not
   the base's. This matches the regulatory record
   posture.
9. **"What about negative amounts? Storno chain
   children carry negative line totals per
   `money.rs` — does the HUF-equivalent
   computation handle negative EUR correctly?"**
   Yes. `Eur(pub i64)` allows negative cents per
   the existing `money.rs` shape; the half-up
   rounding rule in §1.c applies to the absolute
   value with sign preservation (standard
   rounding-mode semantics). PR-44α's
   HUF-conversion test pins the negative-amount
   case explicitly.
10. **"The audit ledger is now load-bearing for
    regulatory recall (the exchange rate lives only
    in the wire bytes, not in a typed audit payload
    field). If the wire bytes are ever lost, the rate
    is unrecoverable."** Defensible. The wire bytes
    ARE the regulatory record (NAV-submitted +
    operator-printed) and the audit ledger's
    `InvoiceSubmissionAttempt` /
    `InvoiceSubmissionResponse` payloads store them
    per ADR-0008 §"Storage". Loss of the wire bytes
    is loss of the regulatory record; the audit
    ledger's tamper-evident chain (ADR-0008) plus
    the mirror file (ADR-0030) plus the bundle
    verifier (ADR-0035) are the existing
    defence-in-depth for that. No new mechanism
    fires.

## Alternatives considered

- **A1 — Wait for the first EUR-customer-facing code
  PR and skip the ADR.** Rejected. ADR-0021 forbids
  soft assertion in advance (CLAUDE.md rule 12),
  but the operator HAS signed an EUR-customer
  scenario (the trigger fired). Filing the ADR
  now is the correct just-in-time response to a
  fired trigger.
- **A2 — Bundle the regulatory pin with the first
  code PR (PR-44α) rather than landing it as a
  doc-only PR.** Rejected. PR-44α is already a
  300-500 LoC lift; bundling the regulatory contract
  with the code PR makes the contract subordinate
  to the code rather than the other way around. The
  posture matches ADR-0035 (verifier ADR before
  verifier code) and ADR-0036 (mirror-invariant
  ADR before classifier extension).
- **A3 — Pin every legal citation now via legal
  consultation before writing the ADR.** Rejected
  on cadence grounds. The `[NEEDS-LEGAL-CHECK]`
  placeholders identify the precise points needing
  legal review; landing the contract structure first
  AND scheduling the legal-review pass against the
  placeholders is faster than blocking the ADR on
  legal calendar. PR-43 lands at "Accepted" because
  the C1-C10 invariants do not depend on the precise
  §-numbers; the §-number resolution moves a
  `[NEEDS-LEGAL-CHECK]` to a citation via amendment
  before any code PR depends on the §-number-specific
  reading.
- **A4 — Make `Currency` an open ISO 4217 string
  rather than a closed enum.** Rejected per §Surfaced
  conflict 3 Reading B (lost). The wire-only
  typed-enum posture (A119 from the 31→46 window)
  applies inside the domain too; the boundary
  conversion is the right place for the string.
- **A5 — Use the European Central Bank (ECB) reference
  rates instead of MNB.** Rejected. The operator's
  scope injection explicitly named "national bank"
  (the MNB). Hungarian regulatory practice for
  HUF-anchored VAT calculation accepts the MNB
  official rate per Áfa tv. §80; ECB rates are not
  the Hungarian-regulator-default source. Switching
  to ECB requires an ADR amendment AND a regulatory
  basis for the change.
- **A6 — Cache the MNB rate aggressively (e.g., per
  day across all tenants).** Out-of-scope at PR-43.
  PR-44β implementation chooses the cache posture;
  §2.b names the per-(currency, date) cache as
  acceptable, the no-cache posture as also
  acceptable, and the choice as named-open.
- **A7 — Reuse the existing `Eur(pub i64)` type
  directly without unifying it with `Huf` under a
  `Currency`-aware shape.** Rejected. Without
  unification, every `total_gross` / `unit_price`
  call site has to disambiguate Huf-vs-Eur in an
  ad-hoc way; the typestate machine (ADR-0009 §2)
  duplicates per currency; the issuance command
  surface diverges. PR-44α's lift unifies them.
  The exact shape of the unification (an enum
  sum, a generic over `Currency`, or a typed
  `Money<C>` wrapper) is named-open in §Open
  question 1.

## Open questions

1. **Exact unification shape for `Huf` + `Eur`.** Is
   `Money` an enum sum (`Money::Huf(Huf) | Money::Eur(Eur)`),
   a generic (`Money<C: Currency>`), or a typed
   wrapper (`Money(Currency, i64)`)? PR-44α decides
   at implementation time; PR-43's pin is only the
   `Currency` enum's shape per §3, not the money
   type's shape. The decision MUST satisfy
   invariants C1, C5, C8, C10.
2. **MNB cache posture.** Per-day cache shared across
   tenants? Per-day cache per tenant? No cache?
   PR-44β decides per §2.b's "named open" note. The
   default lean is **no cache** (re-query MNB on
   every issuance) until operational evidence
   surfaces a need for caching.
3. **`ExchangeRateFetched` audit event.** Should
   PR-44β surface a separate audit event per rate
   fetch (cache-hit vs cache-miss observability)?
   The default per §2.c is **no** (the wire bytes
   already carry the applied rate); §5's deferred
   row names the trigger for revisiting.
4. **Legal review of `[NEEDS-LEGAL-CHECK]` placeholders.**
   PARTIALLY CLOSED at the 2026-05-23 cleanup
   (session 50). **Resolved:** Áfa tv. §80(2) for the
   rate-date alignment rule (MNB official mid-rate of
   the fulfillment date, or D-1 if no rate); Áfa tv.
   §80(1)(g) for the HUF-equivalent printed-invoice
   requirement; NAV XSD field paths for
   `currencyCode` + `exchangeRate` +
   `invoiceVatAmountHUF`; rate precision (6 decimals);
   HUF rounding mode (round-half-even per Áfa
   convention) — superseded the pre-cleanup half-up
   pin. **Still open:** the precise §169 subsection
   that enumerates each printed-invoice field
   individually (rate source name; per-VAT-rate net
   HUF amount); the §172 subsection (if any) carrying
   a storno-currency constraint. The PR-44+ code PRs
   can be implemented against the C1-C11 invariants
   without resolving these residues; no code PR can
   be marked "Accepted" until the §169 + §172
   residues either resolve or get a footnote naming
   the surviving uncertainty.
5. **NAV XSD per-patch-level `vatRateNetAmountHUF` /
   `vatRateVatAmountHUF` cardinality.** §1.b lists
   them as MUST-populate when `currencyCode != HUF`;
   the vendored XSD at PR-44δ time pins the actual
   cardinality (`required` vs `optional with
   conditional asserts`). PR-44δ's golden-XML test
   pins the actual schema requirement.
6. **Operator-config surface for the EUR rate
   refusal-on-MNB-unavailable posture (C2).** A
   future operational case may surface a need for
   an operator-tunable "allow stale-rate issuance
   with audit-marker" escape hatch (mirrors F42 /
   F46 / F50 joint operator-tunable threshold
   posture). Default: no escape hatch; loud-fail
   per C2.
7. **EUR-customer's customer-facing UI text language
   posture.** The §1.a printed-invoice field labels
   (`Összesen HUF-ban`, etc.) are Hungarian. EUR
   customers may be non-Hungarian-speaking; the
   printed-invoice label-language posture is
   named-open. Default per ADR-0017 (Hungarian
   primary): Hungarian labels are correct on the
   regulatory record; an English-label parallel
   variant is a future PR.
8. **Migration sequencing for the `Currency` rename
   on the existing audit-ledger entries' typed
   payloads (if any).** Per §1.b the currency
   appears only inside the wire XML bytes (which
   are byte-stable per C10); but if any typed
   audit payload field carries a "HUF" string, the
   PR-44α rename surfaces it. The expectation
   (verified at PR-44α time) is that no typed
   audit payload field does — the currency lives
   exclusively inside the wire bytes.

## Follow-on PRs unblocked by this decision

- **PR-44α** (domain) — `Currency` enum + `Money`
  unification per §3 + §Open question 1.
- **PR-44β** (rates) — `crates/mnb-rates` per §2.
- **PR-44γ** (issuance) — currency-aware issuance
  commands per §3 refusal posture + C6.
- **PR-44δ** (submission) — XML-builder extension per
  §1.b + C4 + C5.
- **PR-44ε** (UI) — SPA render of currency / rate
  metadata per §1.a (SPA-visible portion) + C7 + C9.

A future **print-rendering PR** (the ADR-0021 deferred
row "Print rendering path") consumes the §1.a printed-
invoice contract for the regulator-facing artifact.

The session-47 handoff carries PR-44α through PR-44ε
as the named PR-43+ candidate space; subsequent
sessions pick from that list per the loop-window
cadence.
