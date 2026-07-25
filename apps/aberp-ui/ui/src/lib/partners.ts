// PR-54 / session-74 — pure-module helpers for the SPA's partner
// management screen + the issue/modification-form typeahead. The
// composer (`composePartnerInputs`), the wire-to-form mapper
// (`formFromPartner`), the typeahead's autofill mapper
// (`buyerFieldsFromPartner`), and the client-side filter
// (`filterPartners`) all live here so vitest can pin them without
// mounting a Svelte component (A156 / A161 / A163 mirror-invariant
// composer-pin pattern).
//
// Pinned by `partners.test.ts`.

import type {
  CustomerType,
  CustomerVatStatusBody,
  Partner,
  PartnerInputs,
  PartnerKind,
} from "./api";

/** S428 — closed-vocab customer-type options + bilingual labels for the
 * PartnerForm dropdown. Order: business segments first, `unset` last (the
 * default). Pinned by `partners.test.ts`. */
export const CUSTOMER_TYPE_OPTIONS: ReadonlyArray<{
  value: CustomerType;
  label: string;
}> = [
  { value: "industrial", label: "Industrial / Ipari" },
  { value: "defense", label: "Defense / Védelmi" },
  { value: "aerospace", label: "Aerospace / Légiipari" },
  { value: "research", label: "Research / Kutatás" },
  { value: "prototype_shop", label: "Prototype shop / Prototípus" },
  { value: "oem", label: "OEM" },
  { value: "consumer", label: "Consumer / Fogyasztói" },
  { value: "unset", label: "Unset / Nincs megadva" },
];

/** S428 — human label for a customer type, or the raw string if unknown. */
export function customerTypeLabel(value: CustomerType | string): string {
  return CUSTOMER_TYPE_OPTIONS.find((o) => o.value === value)?.label ?? value;
}
import {
  applySortDir,
  compareNullishLast,
  localeCompareHu,
  type SortDir,
} from "./list-sort";

/** PR-54 / session-74 — operator-typed form state for the PartnerForm
 * modal. One field per `PartnerInputs` slot; all string-valued so the
 * DOM `bind:value` round-trips cleanly. `kind` is the closed-vocab
 * dropdown's selected literal.
 *
 * PR-97 / ADR-0048 — `customerVatStatus` carries the three-option
 * radio's selected literal. Drives whether `taxNumber` is required
 * (`Domestic`) or disabled (`PrivatePerson` / v1-deferred `Other`)
 * at the form layer. */
export interface PartnerFormState {
  displayName: string;
  legalName: string;
  kind: PartnerKind;
  customerVatStatus: CustomerVatStatusBody;
  /** S428 — closed-vocab customer segment driving the margin profile. */
  customerType: CustomerType;
  taxNumber: string;
  euVatNumber: string;
  addressStreet: string;
  addressPostalCode: string;
  addressCity: string;
  addressCountry: string;
  bankAccount: string;
  contactEmail: string;
  contactPhone: string;
}

/** PR-54 / session-74 — defaults for a freshly-opened PartnerForm in
 * create mode. `kind` defaults to Customer (the dominant operator use
 * case — most partner rows are buyers); `addressCountry` defaults to
 * `Magyarország` since the operator's customer base is Hungarian per
 * the brief. The operator can overwrite both before submit. */
export function emptyPartnerForm(): PartnerFormState {
  return {
    displayName: "",
    legalName: "",
    kind: "Customer",
    // PR-97 / ADR-0048 — defaults to Domestic. Pre-existing partners
    // backfilled the same value via the migration; the dominant
    // operator case is a Hungarian-business buyer.
    customerVatStatus: "Domestic",
    // S428 — defaults to `unset`; the operator picks a segment to drive
    // the margin profile.
    customerType: "unset",
    taxNumber: "",
    euVatNumber: "",
    addressStreet: "",
    addressPostalCode: "",
    addressCity: "",
    addressCountry: "Magyarország",
    bankAccount: "",
    contactEmail: "",
    contactPhone: "",
  };
}

/** PR-54 / session-74 — fold a fetched Partner into the edit-mode form
 * state. Null optional fields collapse to empty strings so the
 * `<input>` bind values stay typed-as-string (a `null` would crash the
 * DOM seam). The reverse direction is [`composePartnerInputs`]. */
export function formFromPartner(partner: Partner): PartnerFormState {
  return {
    displayName: partner.display_name,
    legalName: partner.legal_name,
    kind: partner.kind,
    customerVatStatus: partner.customer_vat_status,
    customerType: partner.customer_type,
    // PR-97 / ADR-0048 — nullable on the wire (PrivatePerson rows
    // store NULL). Collapse to empty string so the DOM input bind
    // value stays typed-as-string.
    taxNumber: partner.tax_number ?? "",
    euVatNumber: partner.eu_vat_number ?? "",
    addressStreet: partner.address_street ?? "",
    addressPostalCode: partner.address_postal_code ?? "",
    addressCity: partner.address_city ?? "",
    addressCountry: partner.address_country ?? "",
    bankAccount: partner.bank_account ?? "",
    contactEmail: partner.contact_email ?? "",
    contactPhone: partner.contact_phone ?? "",
  };
}

/** PR-54 / session-74 — turn the form state into the wire
 * `PartnerInputs` body. Pure function; no side effects. Trims every
 * string field so a `"   "` operator value surfaces as the backend's
 * actionable validation error rather than slipping through (`trim()`
 * mirrors `aberp::partners::inputs_to_normalized` on the Rust side).
 * Empty optional strings collapse to `null` on the wire so the
 * backend's `Option<String>` deserialiser sees absence verbatim. */
export function composePartnerInputs(
  form: PartnerFormState,
): PartnerInputs {
  return {
    display_name: form.displayName.trim(),
    legal_name: form.legalName.trim(),
    kind: form.kind,
    customer_vat_status: form.customerVatStatus,
    customer_type: form.customerType,
    // PR-97 / ADR-0048 — nullable. PrivatePerson rows store NULL; the
    // form's disabled input renders "" which collapses to null here so
    // the backend column sees the absence verbatim. Domestic rows
    // require a non-empty value; the backend's `validate_partner_inputs`
    // surfaces the typed error inline.
    tax_number: emptyToNull(form.taxNumber),
    eu_vat_number: emptyToNull(form.euVatNumber),
    address_street: emptyToNull(form.addressStreet),
    address_postal_code: emptyToNull(form.addressPostalCode),
    address_city: emptyToNull(form.addressCity),
    address_country: emptyToNull(form.addressCountry),
    bank_account: emptyToNull(form.bankAccount),
    contact_email: emptyToNull(form.contactEmail),
    contact_phone: emptyToNull(form.contactPhone),
  };
}

function emptyToNull(s: string): string | null {
  const t = s.trim();
  return t.length > 0 ? t : null;
}

/** PR-54 / session-74 — buyer fields the typeahead's "select a
 * partner" hands the IssueInvoice / ModificationInvoice forms.
 *
 * PR-77 / session-101 — extended to carry the customer-address quartet.
 * NAV's `CUSTOMER_DATA_EXPECTED` business rule rejects any invoice
 * whose buyer is a Hungarian business (DOMESTIC customerVatStatus —
 * today the only path) and whose `<customerAddress>` block is
 * missing; the wire shape on `IssueInvoiceRequest.customer` now
 * carries the address quartet, and the SPA's form pre-populates it
 * from the operator-selected partner so the operator doesn't re-type
 * what the partner record already has. `customerCountryCode` is
 * derived as `HU` whenever the partner is flagged Hungarian (today:
 * every partner — closed-vocab + non-Hungarian buyers are named-
 * deferred). */
export interface BuyerFields {
  customerName: string;
  /** PR-97 / ADR-0048 — closed-vocab buyer-kind, pulled from the
   * partner's stored field so the IssueInvoice form's radio reflects
   * the partner's intrinsic kind. Drives whether the ADÓSZÁM input
   * stays editable on the issue form. */
  customerVatStatus: CustomerVatStatusBody;
  customerTaxNumber: string;
  /** PR-77 / session-101 — derived from the partner's
   * `address_country` (free-form on the partner record). Today every
   * supported buyer is Hungarian and the value is `HU`; if the partner
   * record is missing the country the field falls back to `HU` so the
   * NAV-required code is still present, while the postal-code / city /
   * street fields fall back to empty (the form binding renders the
   * partner gap inline; preflight rejects the submit). */
  customerCountryCode: string;
  customerPostalCode: string;
  customerCity: string;
  customerStreet: string;
  /** ADR-0102 — the partner's `eu_vat_number` (the community VAT
   * number). Seeds the IssueInvoice form's `customerCommunityVatNumber`
   * so an `Other` (EU) buyer's number pre-fills; snapshotted onto the
   * issued invoice at compose time. Empty string when the partner has
   * no EU VAT number. */
  customerCommunityVatNumber: string;
  /** PR-203 / S203 — partner's master `contact_email` (comma-separated
   * canonical form), to seed the IssueInvoice / Modification form's
   * per-invoice email recipient override input. Empty string when the
   * partner has no `contact_email` (PrivatePerson rows often do); the
   * operator can type one for THIS invoice without touching the partner
   * master. */
  emailRecipientOverride: string;
}

/** PR-54 / session-74 — pluck the IssueInvoice/Modification buyer
 * fields from a selected Partner. Per the brief: "buyer auto-populate
 * from the selected partner's data" — the form fields the operator
 * can still tweak before submitting. `legal_name` is the
 * regulatory-compliant string NAV expects on the printed invoice;
 * `display_name` is the operator-friendly label (e.g. "BSCE" vs
 * "Budapesti Sport-Egyesület Kft.") and is NOT what goes on the
 * invoice.
 *
 * PR-77 / session-101 — also pulls the partner's address quartet
 * (street / postal_code / city / country) into the buyer fields so
 * NAV's `<customerAddress>` block is populated end-to-end. The
 * partner record's `address_country` is free-form; we normalise it
 * to the ISO 3166-1 alpha-2 code via `hungarianCountryAliasToCode`
 * (today's closed-vocab maps `Hungary` / `Magyarország` / `HU` →
 * `HU`; everything else falls back to `HU` to preserve the DOMESTIC
 * customerVatStatus assumption until closed-vocab country lands). */
export function buyerFieldsFromPartner(partner: Partner): BuyerFields {
  return {
    customerName: partner.legal_name,
    customerVatStatus: partner.customer_vat_status,
    // PR-97 / ADR-0048 — nullable on the partner record (PrivatePerson
    // rows store NULL). Collapse to empty string for the IssueInvoice
    // form binding; the form's radio + disabled input states reflect
    // the customerVatStatus.
    customerTaxNumber: partner.tax_number ?? "",
    // ADR-0102 — for an `Other` (EU) buyer, pass the partner's actual
    // address country through (best-effort, uppercased) rather than
    // forcing `HU` — an Austrian buyer's `<customerAddress>` must carry
    // `AT`, not `HU`. Domestic/PrivatePerson keep the HU-alias mapping.
    // Full closed-vocab country validation stays deferred (ADR-0102 §8.3).
    customerCountryCode:
      partner.customer_vat_status === "Other"
        ? foreignCountryToCode(partner.address_country)
        : hungarianCountryAliasToCode(partner.address_country),
    customerPostalCode: partner.address_postal_code ?? "",
    customerCity: partner.address_city ?? "",
    customerStreet: partner.address_street ?? "",
    // ADR-0102 — pre-fill the EU community VAT number for Other buyers.
    customerCommunityVatNumber: partner.eu_vat_number ?? "",
    // PR-203 / S203 — pre-fill the IssueInvoice per-invoice email
    // recipient override from the partner master's `contact_email`.
    // The form value is editable in place; editing NEVER writes back to
    // the partner master. Empty string when the partner has no email
    // (the form's input stays blank for the operator to type).
    emailRecipientOverride: partner.contact_email ?? "",
  };
}

/** PR-77 / session-101 — normalise the partner's free-form
 * `address_country` field to NAV's required ISO 3166-1 alpha-2 code.
 * Today's closed vocabulary recognises common Hungarian aliases (the
 * setup-wizard suggests `"Magyarország"`; some partners may have
 * `"Hungary"` or `"HU"`); any other value (or `null`) falls back to
 * `"HU"` — the DOMESTIC customerVatStatus path the emitter
 * unconditionally takes today. Widening to non-Hungarian buyers is
 * named-deferred per the PR-77 handoff (it requires closed-vocab
 * customerVatStatus + a country dropdown in the Partners form).
 *
 * Exported so the SPA unit test can pin every alias the operator may
 * have typed in the Partners form. */
export function hungarianCountryAliasToCode(
  country: string | null | undefined,
): string {
  if (country === null || country === undefined) return "HU";
  const trimmed = country.trim().toLowerCase();
  switch (trimmed) {
    case "":
    case "hu":
    case "magyarorszag":
    case "magyarország":
    case "hungary":
      return "HU";
    default:
      // Fallback: the closed-vocab is intentionally Hungarian-only
      // today. Returning `HU` here preserves the DOMESTIC
      // customerVatStatus assumption — the operator who picked a
      // non-Hungarian partner is in the named-deferred branch and the
      // preflight + validator will catch the actual mismatch
      // downstream (the tax-number shape, etc.).
      return "HU";
  }
}

/** ADR-0102 — best-effort ISO 3166-1 alpha-2 code for a FOREIGN
 * (`Other`) buyer's address country. Unlike `hungarianCountryAliasToCode`
 * (which forces `HU`), this passes the partner's country through: trim +
 * uppercase whatever the operator typed (the country input on the form is
 * editable so they can correct it before issuing). A full closed-vocab
 * country validator is deferred (ADR-0102 §8.3); the Rust preflight
 * `validate_country_code` guard (NAV-adversarial FIX #1) is the load-
 * bearing `[A-Z]{2}` gate that turns a bad country into an operator error
 * rather than a NAV bounce. This keeps an EU buyer's `<customerAddress>`
 * from silently emitting `HU`. */
export function foreignCountryToCode(
  country: string | null | undefined,
): string {
  // A prior version branched on `/^[A-Za-z]{2}$/` but BOTH arms returned
  // `trimmed.toUpperCase()` — a dead branch (NAV-adversarial FIX #1). The
  // uppercase is unconditional; the structural `[A-Z]{2}` validation lives
  // in the Rust preflight, not here (a partner-typed "Austria" must surface
  // as a fixable preflight error, not be silently mangled client-side).
  return (country ?? "").trim().toUpperCase();
}

/** PR-54 / session-74 — client-side admin-mode filter for the
 * PartnersList screen. Case-insensitive substring match on
 * display_name OR legal_name OR tax_number — the three fields the
 * operator is likely to recall when browsing the saved-buyers list.
 * Empty / whitespace-only needle returns the full list unchanged. */
export function filterPartners(rows: Partner[], needle: string): Partner[] {
  const q = needle.trim().toLowerCase();
  if (q.length === 0) return rows;
  return rows.filter((p) => {
    return (
      p.display_name.toLowerCase().includes(q) ||
      p.legal_name.toLowerCase().includes(q) ||
      // PR-97 / ADR-0048 — tax_number is nullable; PrivatePerson rows
      // have NULL and fall through to the display/legal-name match.
      (p.tax_number?.toLowerCase().includes(q) ?? false)
    );
  });
}

// ─── PR-194 / session-194 — sortable columns + kind facet ─────────────
//
// S181 closed the needle-persistence gap; S194 lifts the PartnersList
// to parity with InvoiceList (S119 / S175): clickable column headers
// for Name / Tax number / EU VAT / Kind / City, a closed-vocab Kind
// facet (All / Customer / Supplier / Both), and round-trip persistence
// for both new surfaces. Mirrors `invoice-list.ts` posture; comparators
// are pure (a, b, key, dir) → number so the Svelte renderer can call
// `rows.slice().sort(...)` and rely on Array.prototype.sort's stable
// guarantee (ES2019+).

/** PR-194 — closed-vocab of sortable columns on PartnersList. Mirrors
 * the renderable column set on `PartnersList.svelte`. */
export type PartnerSortKey =
  | "display_name"
  | "tax_number"
  | "eu_vat"
  | "kind"
  | "city";

/** PR-194 — closed-vocab Kind facet. `"All"` short-circuits the gate;
 * the three literal values mirror `PartnerKind`. */
export type PartnerKindFacet = "All" | PartnerKind;

/** PR-194 — quick-filter facet spec. `needle` is the substring search
 * (PR-181); `kind` is the new closed-vocab Kind facet. AND-composed:
 * a row must pass every engaged facet to render. */
export interface PartnerFilterSpec {
  needle: string;
  kind: PartnerKindFacet;
}

/** PR-194 — empty filter (every facet open). */
export const EMPTY_PARTNER_FILTER: PartnerFilterSpec = {
  needle: "",
  kind: "All",
};

/** PR-194 — `true` iff every facet is open. */
export function isPartnerFilterEmpty(spec: PartnerFilterSpec): boolean {
  return spec.needle.trim().length === 0 && spec.kind === "All";
}

/** PR-194 — facet + needle filter for PartnersList. Composes with
 * `filterPartners` (PR-54) so the existing `/`-search behaviour is
 * unchanged when only the needle is set; the kind facet ANDs on top. */
export function filterPartnersWith(
  rows: Partner[],
  spec: PartnerFilterSpec,
): Partner[] {
  const kindGated =
    spec.kind === "All" ? rows : rows.filter((p) => p.kind === spec.kind);
  return filterPartners(kindGated, spec.needle);
}

/** PR-194 — sort index for the closed-vocab Kind. Customer first
 * (dominant operator case), then Supplier, then Both. Pinned by
 * `partners.test.ts`. */
function partnerKindIndex(kind: PartnerKind): number {
  switch (kind) {
    case "Customer":
      return 0;
    case "Supplier":
      return 1;
    case "Both":
      return 2;
  }
}

/** PR-194 — `id` tiebreaker. Ascending regardless of the user-selected
 * sort dir so two rows with equal sort values land in a reproducible
 * order across refreshes (mirror of `invoice-list.ts ::
 * invoiceIdTiebreak`). */
function partnerIdTiebreak(a: Partner, b: Partner): number {
  if (a.id < b.id) return -1;
  if (a.id > b.id) return 1;
  return 0;
}

/** PR-194 — pure comparator. Returns a `(a, b) → number` suitable for
 * `Array.prototype.sort`. Locale-aware (Hungarian) on string columns
 * so accented characters land in operator-natural order. Nulls-last
 * on the nullable columns (tax_number / eu_vat / city) regardless of
 * direction — mirror of the invoice-list convention. Ties go to the
 * partner `id` ascending so render order is reproducible. */
export function comparePartners(
  a: Partner,
  b: Partner,
  key: PartnerSortKey,
  dir: SortDir,
): number {
  const nullCmp = partnerNullsLast(a, b, key);
  if (nullCmp !== null) {
    if (nullCmp !== 0) return nullCmp;
    return partnerIdTiebreak(a, b);
  }
  const raw = partnerRawCompare(a, b, key);
  if (raw !== 0) return applySortDir(raw, dir);
  return partnerIdTiebreak(a, b);
}

function partnerNullsLast(
  a: Partner,
  b: Partner,
  key: PartnerSortKey,
): number | null {
  switch (key) {
    case "tax_number":
      return compareNullishLast(a.tax_number, b.tax_number);
    case "eu_vat":
      return compareNullishLast(a.eu_vat_number, b.eu_vat_number);
    case "city":
      return compareNullishLast(a.address_city, b.address_city);
    default:
      return null;
  }
}

function partnerRawCompare(
  a: Partner,
  b: Partner,
  key: PartnerSortKey,
): number {
  switch (key) {
    case "display_name":
      return localeCompareHu(a.display_name, b.display_name);
    case "tax_number":
      // Non-null assertion safe — partnerNullsLast returned non-null
      // only when both sides have a value.
      return localeCompareHu(a.tax_number as string, b.tax_number as string);
    case "eu_vat":
      return localeCompareHu(
        a.eu_vat_number as string,
        b.eu_vat_number as string,
      );
    case "kind":
      return partnerKindIndex(a.kind) - partnerKindIndex(b.kind);
    case "city":
      return localeCompareHu(
        a.address_city as string,
        b.address_city as string,
      );
  }
}

/** PR-194 — runtime list of legal sort keys (mirrors `PartnerSortKey`).
 * Persistence validators use this to discard stale keys from a future
 * rename; kept here next to the union so the two surfaces stay in
 * sync. */
export const LEGAL_PARTNER_SORT_KEYS: readonly PartnerSortKey[] = [
  "display_name",
  "tax_number",
  "eu_vat",
  "kind",
  "city",
];

/** PR-194 — runtime list of legal Kind facet values. */
export const LEGAL_PARTNER_KIND_FACETS: readonly PartnerKindFacet[] = [
  "All",
  "Customer",
  "Supplier",
  "Both",
];

/** PR-54 / session-74 — typed 400 validation body parser. Mirrors the
 * shape of `parseSetupSellerInfoErrorBody` (A157): peel the JSON
 * object out of the Tauri-wrapped error string, accept iff the
 * `error` discriminant matches `"validation_failed"`. Returns `null`
 * for any other shape so the caller falls back to a generic raw-
 * string display. */
export function parsePartnerValidationError(
  raw: string,
): { error: "validation_failed"; fields: Array<{ field: string; message: string }> } | null {
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start < 0 || end <= start) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.slice(start, end + 1));
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const obj = parsed as Record<string, unknown>;
  if (obj.error !== "validation_failed") return null;
  if (!Array.isArray(obj.fields)) return null;
  const fields: Array<{ field: string; message: string }> = [];
  for (const entry of obj.fields) {
    if (typeof entry !== "object" || entry === null) return null;
    const e = entry as Record<string, unknown>;
    if (typeof e.field !== "string" || typeof e.message !== "string") {
      return null;
    }
    fields.push({ field: e.field, message: e.message });
  }
  return { error: "validation_failed", fields };
}
