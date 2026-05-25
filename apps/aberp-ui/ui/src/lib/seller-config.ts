// PR-51 / session-71 — form-to-request composer for the
// SellerConfigWizard. Mirrors the PR-46α `setup-credentials.ts`
// A156/A161/A163 composer-pin pattern: operator-facing form state is
// one shape (camelCase, with required fields typed as `string`); the
// backend's `POST /api/setup-seller-info` body is another (snake_case,
// nested address + bank objects). Splitting the composer out of the
// Svelte component keeps the component-test runner named-deferred per
// CLAUDE.md rule 2 — vitest pins the composer + validator without
// mounting the wizard.

import type {
  SetupSellerInfoErrorBody,
  SetupSellerInfoRequest,
} from "./api";

/** Operator-facing form state for the SellerConfigWizard. Field
 * names match the form labels (camelCase per the rest of the SPA);
 * the composer snake-cases them on the way to the backend wire shape.
 *
 * Required identity fields are typed `string` (not `string | null`)
 * because the validator's "non-empty after trim" rule is the contract
 * — an empty string is invalid input, not absence. Optional fields
 * (`euVatNumber`, all four bank fields) are typed `string` too with
 * empty-string as the "operator skipped this" sentinel; the composer
 * folds empty strings into `null` on the wire side so the backend's
 * `Option<String>` deserialiser sees the right shape. */
export interface SellerConfigForm {
  legalName: string;
  taxNumber: string;
  euVatNumber: string;
  addressCountryCode: string;
  addressPostalCode: string;
  addressCity: string;
  addressStreet: string;
  bankAccountNumber: string;
  iban: string;
  bankName: string;
  swiftBic: string;
}

/** PR-51 / session-71 — default state for a fresh wizard mount.
 * Country code defaults to `"HU"` (the Hungarian ISO 3166-1 alpha-2
 * code, matching the schema NAV expects); every other field starts
 * blank so the operator types each one explicitly. */
export const DEFAULT_SELLER_CONFIG_FORM: SellerConfigForm = {
  legalName: "",
  taxNumber: "",
  euVatNumber: "",
  addressCountryCode: "HU",
  addressPostalCode: "",
  addressCity: "",
  addressStreet: "",
  bankAccountNumber: "",
  iban: "",
  bankName: "",
  swiftBic: "",
};

/** Validation result for the form. `null` per-field means the field
 * is acceptable; a string is the operator-facing inline-error
 * message. */
export interface SellerConfigValidation {
  legalName: string | null;
  taxNumber: string | null;
  addressCountryCode: string | null;
  addressPostalCode: string | null;
  addressCity: string | null;
  addressStreet: string | null;
  /** `true` iff every per-field error is `null`. */
  ok: boolean;
}

/** PR-51 / session-71 — Hungarian ADÓSZÁM shape `xxxxxxxx-y-zz`
 * matcher. Same contract as the Rust-side
 * `nav_xml::parse_hungarian_tax_number`. Client-side coverage saves
 * a round-trip on the most common operator typo (missing dash, extra
 * digit) — the backend is still the authoritative gate. */
const HUNGARIAN_TAX_NUMBER_PATTERN = /^[0-9]{8}-[0-9]-[0-9]{2}$/;

/** Per-field validator. Optional fields (eu VAT, bank.*) have no
 * shape gate here — the operator may legitimately leave them blank,
 * and the backend accepts any string. */
export function validateSellerConfig(
  form: SellerConfigForm,
): SellerConfigValidation {
  const legalName =
    form.legalName.trim().length === 0 ? "Legal name is required" : null;
  let taxNumber: string | null = null;
  const taxTrimmed = form.taxNumber.trim();
  if (taxTrimmed.length === 0) {
    taxNumber =
      "Tax number (ADÓSZÁM) is required — Hungarian shape `xxxxxxxx-y-zz`, e.g. `24904362-2-41`";
  } else if (!HUNGARIAN_TAX_NUMBER_PATTERN.test(taxTrimmed)) {
    taxNumber =
      "Tax number must match Hungarian ADÓSZÁM shape `xxxxxxxx-y-zz` (e.g. `24904362-2-41`)";
  }
  const addressCountryCode =
    form.addressCountryCode.trim().length === 0
      ? "Country code is required (default: HU)"
      : null;
  const addressPostalCode =
    form.addressPostalCode.trim().length === 0
      ? "Postal code is required"
      : null;
  const addressCity =
    form.addressCity.trim().length === 0 ? "City is required" : null;
  const addressStreet =
    form.addressStreet.trim().length === 0 ? "Street is required" : null;
  const ok =
    legalName === null &&
    taxNumber === null &&
    addressCountryCode === null &&
    addressPostalCode === null &&
    addressCity === null &&
    addressStreet === null;
  return {
    legalName,
    taxNumber,
    addressCountryCode,
    addressPostalCode,
    addressCity,
    addressStreet,
    ok,
  };
}

/** Compose the wire request body from the form state. Mirror of the
 * Rust-side `serve::SetupSellerInfoRequest` (snake_case + nested
 * address/bank objects). Pre-condition:
 * `validateSellerConfig(form).ok` is `true`. Optional fields with
 * empty-string values fold to `null` so the backend's
 * `Option<String>` deserialiser does the right thing. */
export function composeSellerConfigBody(
  form: SellerConfigForm,
): SetupSellerInfoRequest {
  return {
    legal_name: form.legalName.trim(),
    tax_number: form.taxNumber.trim(),
    eu_vat_number: blankToNull(form.euVatNumber),
    address: {
      country_code: form.addressCountryCode.trim(),
      postal_code: form.addressPostalCode.trim(),
      city: form.addressCity.trim(),
      street: form.addressStreet.trim(),
    },
    bank: {
      account_number: blankToNull(form.bankAccountNumber),
      iban: blankToNull(form.iban),
      name: blankToNull(form.bankName),
      swift_bic: blankToNull(form.swiftBic),
    },
  };
}

function blankToNull(value: string): string | null {
  const trimmed = value.trim();
  return trimmed.length === 0 ? null : trimmed;
}

/** PR-51 / session-71 — parse the typed 400 body emitted by the
 * backend's seller-info route. The body lives inside a forwarded
 * error string of shape
 * `"backend returned 400 Bad Request for /api/setup-seller-info: <json>"`
 * — the substring after the `: ` is the JSON body. Returns the
 * field-level errors when the body matches the expected shape,
 * `null` otherwise (so the wizard can fall back to the raw error
 * banner instead of pretending fields are valid). */
export function parseSetupSellerInfoErrorBody(
  errorMessage: string,
): SetupSellerInfoErrorBody | null {
  // The Tauri shell prefixes 4xx errors with
  // `"backend returned <status> for <path>: "`; extract the JSON tail.
  const marker = "/api/setup-seller-info: ";
  const idx = errorMessage.indexOf(marker);
  const jsonStart = idx >= 0 ? idx + marker.length : -1;
  const candidate =
    jsonStart >= 0 ? errorMessage.slice(jsonStart) : errorMessage;
  let parsed: unknown;
  try {
    parsed = JSON.parse(candidate);
  } catch {
    return null;
  }
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    !("error" in parsed) ||
    !("fields" in parsed)
  ) {
    return null;
  }
  const body = parsed as { error: unknown; fields: unknown };
  if (body.error !== "validation_failed" || !Array.isArray(body.fields)) {
    return null;
  }
  const fields: { field: string; message: string }[] = [];
  for (const raw of body.fields) {
    if (
      typeof raw === "object" &&
      raw !== null &&
      "field" in raw &&
      "message" in raw &&
      typeof (raw as { field: unknown }).field === "string" &&
      typeof (raw as { message: unknown }).message === "string"
    ) {
      const r = raw as { field: string; message: string };
      fields.push({ field: r.field, message: r.message });
    } else {
      return null;
    }
  }
  return { error: "validation_failed", fields };
}
