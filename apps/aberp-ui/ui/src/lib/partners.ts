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

import type { Partner, PartnerInputs, PartnerKind } from "./api";

/** PR-54 / session-74 — operator-typed form state for the PartnerForm
 * modal. One field per `PartnerInputs` slot; all string-valued so the
 * DOM `bind:value` round-trips cleanly. `kind` is the closed-vocab
 * dropdown's selected literal. */
export interface PartnerFormState {
  displayName: string;
  legalName: string;
  kind: PartnerKind;
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
    taxNumber: partner.tax_number,
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
    tax_number: form.taxNumber.trim(),
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
 * Customer name + tax number are the only two fields the existing
 * wire shape carries (`IssueInvoiceRequest.customer` is just
 * `{taxNumber, name}` today per session-73's surgical posture). The
 * SPA's form bindings consume these two values verbatim. */
export interface BuyerFields {
  customerName: string;
  customerTaxNumber: string;
}

/** PR-54 / session-74 — pluck the IssueInvoice/Modification buyer
 * fields from a selected Partner. Per the brief: "buyer auto-populate
 * from the selected partner's data" — the form fields the operator
 * can still tweak before submitting. `legal_name` is the
 * regulatory-compliant string NAV expects on the printed invoice;
 * `display_name` is the operator-friendly label (e.g. "BSCE" vs
 * "Budapesti Sport-Egyesület Kft.") and is NOT what goes on the
 * invoice. */
export function buyerFieldsFromPartner(partner: Partner): BuyerFields {
  return {
    customerName: partner.legal_name,
    customerTaxNumber: partner.tax_number,
  };
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
      p.tax_number.toLowerCase().includes(q)
    );
  });
}

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
