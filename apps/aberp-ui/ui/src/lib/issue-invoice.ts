// PR-44ζ / session-59 — form-state + form-to-request-body composer
// for the SPA's IssueInvoice form. Kept in a pure module (no Svelte
// runes; no DOM) so the composer is testable under vitest without
// mounting a component.
//
// The composer is the load-bearing seam between the operator-typed
// form values and the wire shape `serve::IssueInvoiceRequest`
// expects: the backend Deserializer is strict (uppercase currency,
// camelCase JSON field names), and a regression that mis-mints any
// of those would surface as a 400 rather than a silent issuance with
// wrong data.
//
// Pinned by `issue-invoice.test.ts` per the A156 / A161 mirror-
// invariant precedent.

import type { Currency, IssueInvoiceRequest } from "./api";
import { parseAmountToMinor } from "./format";
import {
  addDays,
  comfortZone,
  DEFAULT_PAYMENT_OFFSET_DAYS,
  overrideKindForZone,
  todayLocalIso,
  type DeliveryDateOverride,
  type IsoDate,
} from "./invoice-dates";

/** PR-88 / session-113 — per-line form state.
 *
 * `unitPriceInput` is the OPERATOR-TYPED RAW STRING from the form's
 * unit-price input. The composer converts it to integer minor units
 * via [`parseAmountToMinor`] at submit time, using the form's
 * currency to drive the major→minor scaling (`340` typed under EUR
 * becomes 34000 cents; under HUF, 340 forints).
 *
 * Pre-PR-88 this field was `unitPriceMinor: number` bound directly
 * to a `type="number"` input — the operator's typed digits were
 * persisted verbatim as MINOR units. That worked for HUF (HUF is
 * 0-decimal so major == minor) but produced a 100× underbill for
 * EUR: `340` typed → 340 cents on the wire = 3.40 EUR instead of
 * 340.00 EUR. Ervin issued one wrong-amount invoice in live test
 * before catching it. The fix is to read raw operator input as a
 * string and convert at compose time so the major-unit
 * interpretation is canonical.
 *
 * `quantity` and `vatRatePercent` remain integer because the form's
 * `<input type="number">` for those fields is unambiguous (no
 * decimal-separator ambiguity, no minor-unit scaling). */
export interface LineFormState {
  description: string;
  quantity: number;
  unitPriceInput: string;
  vatRatePercent: number;
  /** PR-82 — operator-typed per-line note ("Megjegyzés"). Empty
   * string when blank; `composeIssueInvoiceBody` normalises to
   * `null` on the wire so the backend sees a clean "no note"
   * signal. Recipient-facing only — NEVER reaches the NAV XML. */
  note: string;
}

/** PR-44ζ — top-level form state. Captures every operator-typed
 * value the form exposes; the composer reshapes it into the wire
 * `IssueInvoiceRequest`.
 *
 * PR-53 / session-73 — supplier fields removed from the form (the
 * backend now reads seller identity from the per-tenant
 * `seller.toml` populated via the wizard). Operator-typed values are
 * customer + currency + line items only. */
export interface IssueInvoiceFormState {
  customerTaxNumber: string;
  customerName: string;
  /** PR-77 / session-101 — customer-address quartet. Populated from
   * the operator-selected partner via `buyerFieldsFromPartner`
   * (PR-54 buyer combobox). Required for any Hungarian-business
   * buyer; preflight rejects an invoice whose customer.address is
   * absent or any of these four fields is blank-after-trim. The
   * `customerCountryCode` is locked to `HU` for every Hungarian-
   * DOMESTIC buyer today; widening to non-Hungarian buyers is named-
   * deferred per the PR-77 handoff. */
  customerCountryCode: string;
  customerPostalCode: string;
  customerCity: string;
  customerStreet: string;
  currency: Currency;
  lines: LineFormState[];
  /** PR-73 / ADR-0040 §addendum — operator-selected bank account id
   * (the `bnk_<26-char>` value from `listSellerBanks`). `null`
   * means "use the per-currency default" — the SPA's bank picker
   * defaults this to the entry with `is_default: true` for the
   * current `currency` but lets the operator switch. The composer
   * emits `null` as `bankAccountId: null` on the wire; the backend
   * resolver treats `null` the same as missing-field and falls back
   * to the per-currency default. */
  bankAccountId: string | null;
  /** PR-82 — operator-typed per-invoice global note ("Megjegyzés").
   * Empty string when the textarea is blank; the composer
   * normalises to `null` on the wire so the backend sees a clean
   * "no note" signal. Recipient-facing only — NEVER reaches the
   * NAV InvoiceData XML. */
  invoiceNote: string;
  /** PR-84 — operator-visible invoice date (Számla kelte). Read-only
   * in the UI; defaulted to today's local date. The server stamps the
   * TRUE issue date at issuance time (immutable, never trusts the
   * client clock); this is purely a display default for the form's
   * date section + an anchor for the payment-deadline and delivery-
   * date pickers. Canonical YYYY-MM-DD. */
  invoiceDate: IsoDate;
  /** PR-84 — operator-supplied payment deadline (Fizetési határidő).
   * Bidirectional: the form exposes both an offset-days input and an
   * absolute date picker; the two update each other live. This field
   * carries the resolved absolute date (the offset is derived on
   * render via `daysBetween(invoiceDate, paymentDeadline)`). */
  paymentDeadline: IsoDate;
  /** PR-84 — operator-chosen delivery / fulfillment date (Teljesítési
   * dátum). REGULATORY: drives the NAV `<invoiceDeliveryDate>` field.
   * Defaults to invoiceDate; the operator can pick any date but
   * out-of-range choices (before invoiceDate OR after paymentDeadline)
   * trigger an inline "Are you sure?" confirm that captures the audit
   * override. */
  deliveryDate: IsoDate;
  /** PR-84 — comfort-zone audit discriminant the operator has
   * confirmed for the current `deliveryDate`. `null` when the
   * delivery date is in range (default, no audit flag); a non-null
   * value persists across edits until the operator picks a different
   * date OR confirms the new override. The composer stamps the
   * current value verbatim into the wire body's `deliveryDateOverride`
   * field. */
  deliveryDateOverride: DeliveryDateOverride;
}

/** PR-44ζ — sensible defaults for an empty form. The 27% VAT rate is
 * the Hungarian standard rate; HUF is the default currency (matches
 * the CLI's default). One empty line is included so the form is
 * editable on first paint without a separate "+ Add line" click. */
export function emptyForm(): IssueInvoiceFormState {
  const today = todayLocalIso();
  // PR-84 — payment deadline seeds to `today + DEFAULT_PAYMENT_OFFSET_DAYS`
  // (a sensible business default per the brief; 8 days). The form's
  // bidirectional control lets the operator edit either side of the
  // pair. The unwrap is safe — `todayLocalIso()` produces a well-formed
  // YYYY-MM-DD and `addDays` only returns null on malformed input.
  const defaultDeadline = addDays(today, DEFAULT_PAYMENT_OFFSET_DAYS) ?? today;
  return {
    customerTaxNumber: "",
    customerName: "",
    // PR-77 / session-101 — customer address fields seed to empty
    // strings; the operator-selected partner populates them via
    // `buyerFieldsFromPartner`. A required-by-NAV submission with any
    // of these blank trips the preflight gate.
    customerCountryCode: "",
    customerPostalCode: "",
    customerCity: "",
    customerStreet: "",
    currency: "HUF",
    lines: [emptyLine()],
    // PR-73 — `null` means "use the per-currency default"; the
    // IssueInvoice.svelte effect re-runs whenever `currency` changes
    // and pre-populates this from the currency's `is_default` entry.
    bankAccountId: null,
    // PR-82 — invoice-level note seeds blank; operator opt-in.
    invoiceNote: "",
    // PR-84 — three invoice-date fields seeded for the form's first
    // paint. The display value mirrors today; the server stamps the
    // real issue date at issuance time. Delivery date defaults to the
    // invoice date (the common case — supply delivered same day as
    // invoicing); operator can pick any date with the comfort-zone
    // confirm UX.
    invoiceDate: today,
    paymentDeadline: defaultDeadline,
    deliveryDate: today,
    deliveryDateOverride: null,
  };
}

/** PR-44ζ — sensible defaults for a freshly-added line.
 *
 * PR-88 / session-113 — `unitPriceInput` seeds to an empty string;
 * the form's required-attribute on the text input forces the
 * operator to type a value before submission. A 0-default would
 * silently round-trip through the parser as `null` (rejected via
 * the empty-string arm) which the backend preflight then catches
 * as `LineItemUnitPriceNonPositive` — but presenting an empty
 * input matches the "blank canvas" UX a fresh line should have. */
export function emptyLine(): LineFormState {
  return {
    description: "",
    quantity: 1,
    unitPriceInput: "",
    vatRatePercent: 27,
    // PR-82 — per-line note seeds blank; operator opt-in.
    note: "",
  };
}

/** PR-84 — bidirectional payment-deadline helper. Given the form's
 * current `invoiceDate` and a new `offsetDays` value the operator just
 * typed, return the resolved absolute `paymentDeadline`. Returns null
 * on malformed input. The companion direction (operator picks an
 * absolute date) just sets `paymentDeadline` directly; the offset is
 * a derived read via `daysBetween(invoiceDate, paymentDeadline)`. */
export function paymentDeadlineFromOffset(
  invoiceDate: IsoDate,
  offsetDays: number,
): IsoDate | null {
  return addDays(invoiceDate, offsetDays);
}

/** PR-84 — classify a candidate delivery date against the form's
 * comfort zone [invoiceDate, paymentDeadline] and return the audit-
 * wire discriminant the form should stamp. `null` means in-range (no
 * audit flag, no confirm prompt needed); non-null means the operator
 * picked an out-of-range date and the form should surface the inline
 * "Are you sure?" confirm before stamping this on the wire body.
 *
 * Returns `null` on malformed input — the form's per-field validator
 * surfaces the precise gap separately. */
export function deliveryDateOverrideFor(
  invoiceDate: IsoDate,
  paymentDeadline: IsoDate,
  deliveryDate: IsoDate,
): DeliveryDateOverride {
  const zone = comfortZone(invoiceDate, paymentDeadline, deliveryDate);
  if (zone === null) return null;
  return overrideKindForZone(zone);
}

/** PR-50 / session-70 — typed `missing_seller_config` error body the
 * backend's `serve::handle_issue_invoice` 400 surface emits when
 * `validate_supplier_info` rejects the operator-typed tax number.
 * Mirrors `serve::TypedErrorBody` on the Rust side.
 *
 * The SPA's inline-error renderer detects this discriminant and
 * surfaces the `config_path` + `sample_path` as actionable hints so
 * the operator knows where the eventual config home lives (PR-51's
 * wizard destination) without having to dig through the close-handoff
 * notes. */
export interface MissingSellerConfigError {
  /** Discriminant — exact string the backend serializes. */
  error: "missing_seller_config";
  /** Human-readable diagnostic carrying the rejected input + the
   * shape expectation. Surfaced verbatim by the renderer. */
  message: string;
  /** Per-tenant `seller.toml` path the SPA shows as the "fill in
   * here" pointer. PR-51 wires this destination; today the message
   * still names it as the forward-looking config home. */
  config_path: string;
  /** Repo-relative `samples/seller.toml.example` path the SPA shows
   * as the template source. */
  sample_path: string;
}

/** PR-50 / session-70 — parse the raw error string the Tauri forward
 * helper hands back (shape:
 * `"backend returned 400 Bad Request for /invoices/issue: {json}"`)
 * into the typed `missing_seller_config` body when present.
 *
 * Returns `null` for any other shape (network error, 500, 400 without
 * the typed discriminant). The caller falls back to displaying the
 * raw message in that case.
 *
 * Hand-rolled JSON extraction (substring + JSON.parse) rather than
 * pulling in a parser dep — the wrapping format is fixed and the
 * `{ ... }` substring is unambiguous (the backend's body is a JSON
 * object). */
export function parseMissingSellerConfigError(
  raw: string,
): MissingSellerConfigError | null {
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
  if (obj.error !== "missing_seller_config") return null;
  if (
    typeof obj.message !== "string" ||
    typeof obj.config_path !== "string" ||
    typeof obj.sample_path !== "string"
  ) {
    return null;
  }
  return {
    error: "missing_seller_config",
    message: obj.message,
    config_path: obj.config_path,
    sample_path: obj.sample_path,
  };
}

/** PR-69 / session-91 — closed-vocab pre-issuance error variant the
 * backend's `validate_invoice_preflight` enumerates per ADR-0038.
 * Mirrors the `kind` field of `serve::PreflightErrorItem` on the
 * Rust side. New variant requires a paired pin: extend this union
 * AND add a vitest case in `issue-invoice.test.ts`. */
export type InvoicePreflightErrorKind =
  | "CustomerNameEmpty"
  | "CustomerTaxNumberMissing"
  | "CustomerTaxNumberMalformed"
  | "InvoiceLinesEmpty"
  | "LineItemDescriptionEmpty"
  | "LineItemQuantityZero"
  | "LineItemUnitPriceNonPositive"
  | "LineItemVatRateUnknown"
  | "SellerBankMissingForCurrency"
  | "SellerBankCurrencyMismatch";

/** PR-69 / session-91 — one operator-correctable preflight error
 * returned by `POST /invoices/issue` when the request body fails the
 * pre-issuance shape gate (ADR-0038). The SPA's IssueInvoice form
 * renders these inline at `field_path`'s input (red border + the
 * Hungarian + English message stacked beneath the input). */
export interface InvoicePreflightErrorItem {
  /** Closed-vocab discriminant. The renderer pattern-matches on this
   * for variant-specific UI affordances (e.g. linking the rejected
   * VAT rate to the allowed-set hint). */
  kind: InvoicePreflightErrorKind;
  /** Dotted path into the wire shape (`customer.name`,
   * `lines[2].vatRatePercent`, …). Used to route the inline error to
   * the right input element. */
  field_path: string;
  /** Hungarian operator-facing message — rendered verbatim. */
  message_hu: string;
  /** English developer / debug message — rendered alongside HU. */
  message_en: string;
}

/** PR-69 / session-91 — typed 400 body the backend's
 * `serve::handle_issue_invoice` emits when the preflight validator
 * (`validate_invoice_preflight`, ADR-0038) returns a non-empty error
 * vec. Sibling of [`MissingSellerConfigError`] with a `errors` array
 * instead of a single message so the operator sees every problem at
 * once.
 *
 * The outer `error` discriminant distinguishes a preflight 400 from
 * the PR-50 `missing_seller_config` 400 and from the legacy plain
 * 400 (`validate_issue_request`'s empty-string surface). */
export interface InvoicePreflightErrorBody {
  error: "invoice_preflight_failed";
  errors: InvoicePreflightErrorItem[];
}

/** PR-69 / session-91 — parse the raw error string the Tauri
 * forward helper hands back (shape: `"backend returned 400 Bad
 * Request for /invoices/issue: {json}"`) into the typed preflight
 * body when present.
 *
 * Returns `null` for any other shape (network error, 500, plain 400
 * without the typed discriminant, `missing_seller_config` 400). The
 * caller then either tries `parseMissingSellerConfigError` (which
 * has the same return-null-on-mismatch posture) or falls back to
 * the raw message.
 *
 * Same hand-rolled JSON extraction as `parseMissingSellerConfigError`
 * — substring + JSON.parse, no dep. */
export function parseInvoicePreflightErrors(
  raw: string,
): InvoicePreflightErrorBody | null {
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
  if (obj.error !== "invoice_preflight_failed") return null;
  if (!Array.isArray(obj.errors)) return null;
  const items: InvoicePreflightErrorItem[] = [];
  for (const candidate of obj.errors) {
    if (typeof candidate !== "object" || candidate === null) return null;
    const item = candidate as Record<string, unknown>;
    if (
      typeof item.kind !== "string" ||
      typeof item.field_path !== "string" ||
      typeof item.message_hu !== "string" ||
      typeof item.message_en !== "string"
    ) {
      return null;
    }
    if (!isKnownPreflightKind(item.kind)) return null;
    items.push({
      kind: item.kind,
      field_path: item.field_path,
      message_hu: item.message_hu,
      message_en: item.message_en,
    });
  }
  return { error: "invoice_preflight_failed", errors: items };
}

/** PR-69 / session-91 — closed-vocab guard. A backend variant the SPA
 * does not know about should fail loud rather than render as
 * `(unknown error)` — the renderer needs to know about every variant
 * so the inline-error UI is exhaustive. */
function isKnownPreflightKind(s: string): s is InvoicePreflightErrorKind {
  switch (s) {
    case "CustomerNameEmpty":
    case "CustomerTaxNumberMissing":
    case "CustomerTaxNumberMalformed":
    case "InvoiceLinesEmpty":
    case "LineItemDescriptionEmpty":
    case "LineItemQuantityZero":
    case "LineItemUnitPriceNonPositive":
    case "LineItemVatRateUnknown":
    case "SellerBankMissingForCurrency":
    case "SellerBankCurrencyMismatch":
      return true;
    default:
      return false;
  }
}

/** PR-69 / session-91 — given a `field_path` returned by the backend
 * preflight, extract a stable DOM-input identifier the IssueInvoice
 * form uses to target the inline-error rendering. Customer paths
 * map to bare field names; line paths to a `(lineIndex, field)`
 * tuple.
 *
 * Returns `null` for any path shape outside the closed-vocab — the
 * renderer then renders the error in the general error block rather
 * than dropping it. Same posture as the closed-vocab kind guard
 * above. */
export type PreflightFieldTarget =
  | { kind: "customer"; field: "name" | "taxNumber" }
  | { kind: "lines" }
  | { kind: "bankAccountId" }
  | {
      kind: "line";
      lineIndex: number;
      field: "description" | "quantity" | "unitPrice" | "vatRatePercent";
    };

export function targetForFieldPath(
  fieldPath: string,
): PreflightFieldTarget | null {
  if (fieldPath === "customer.name") {
    return { kind: "customer", field: "name" };
  }
  if (fieldPath === "customer.taxNumber") {
    return { kind: "customer", field: "taxNumber" };
  }
  if (fieldPath === "lines") {
    return { kind: "lines" };
  }
  if (fieldPath === "bankAccountId") {
    return { kind: "bankAccountId" };
  }
  // Match `lines[N].field` where N is a non-negative integer and
  // field is one of the four line-level closed-vocab field names.
  const lineMatch = /^lines\[(\d+)\]\.(description|quantity|unitPrice|vatRatePercent)$/.exec(
    fieldPath,
  );
  if (lineMatch) {
    return {
      kind: "line",
      lineIndex: Number(lineMatch[1]),
      field: lineMatch[2] as
        | "description"
        | "quantity"
        | "unitPrice"
        | "vatRatePercent",
    };
  }
  return null;
}

/** PR-44ζ — turn the form state into the wire `IssueInvoiceRequest`.
 * Pure function; no side effects. The trim on string fields mirrors
 * the backend's `validate_issue_request` (which `.trim()`-checks the
 * same fields) so a form value of `"   "` surfaces as a 400 with the
 * actionable "required" message rather than passing pre-validation
 * and failing deeper. */
export function composeIssueInvoiceBody(
  form: IssueInvoiceFormState,
): IssueInvoiceRequest {
  return {
    customer: {
      taxNumber: form.customerTaxNumber.trim(),
      name: form.customerName.trim(),
      // PR-77 / session-101 — customer address quartet. Always emit
      // the field when ANY of the four sub-strings is non-empty after
      // trim; the backend preflight rejects partially-blank addresses
      // explicitly so the operator sees the precise gap. If every
      // sub-string is blank we omit the field — that surfaces as
      // `CustomerAddressMissing` on the preflight rather than as a
      // body with four empty strings (cleaner operator message).
      address: composeCustomerAddress(form),
    },
    lines: form.lines.map((l) => ({
      description: l.description.trim(),
      quantity: l.quantity,
      // PR-88 / session-113 — parse the operator-typed string into
      // integer minor units using the form's currency. Bare ints
      // are interpreted as WHOLE major units (`340` EUR = 34000
      // cents; `340` HUF = 340 forints). A parse failure surfaces
      // as 0 on the wire so the backend preflight's
      // `LineItemUnitPriceNonPositive` rule renders the inline
      // error at this line's unit-price input — the operator gets
      // the existing PR-69 actionable message rather than a silent
      // bad-amount issuance. See [`parseAmountToMinor`] for the
      // closed grammar.
      unitPrice: parseAmountToMinor(l.unitPriceInput, form.currency) ?? 0,
      vatRatePercent: l.vatRatePercent,
      // PR-82 — per-line buyer note. Trim + normalise empty to
      // `null` so the backend's preflight / persistence path sees a
      // clean "no note" signal rather than a blank-string row.
      note: blankToNull(l.note),
    })),
    currency: form.currency,
    // PR-73 / ADR-0040 §addendum — operator-selected bank account.
    // Sent verbatim; `null` lets the backend fall back to the per-
    // currency default. Empty-string is normalised to `null` so the
    // backend resolver sees a clean "no selection" signal.
    bankAccountId:
      form.bankAccountId !== null && form.bankAccountId.trim() !== ""
        ? form.bankAccountId
        : null,
    // PR-82 — per-invoice global buyer note. Same blank-to-null
    // normalisation as per-line notes; the backend's `Option<String>`
    // deserialiser treats `null` and an absent field identically.
    invoiceNote: blankToNull(form.invoiceNote),
    // PR-84 — operator-supplied payment deadline + delivery date go
    // on the wire verbatim. Both are canonical YYYY-MM-DD strings.
    // The wire body does NOT carry the form's `invoiceDate` field —
    // the server stamps the immutable issue date from its own clock
    // (per ADR-0007 §"Operator-as-threat-actor"); the form's display
    // value is purely UX-anchoring.
    paymentDeadline: form.paymentDeadline,
    deliveryDate: form.deliveryDate,
    // PR-84 — comfort-zone audit discriminant. `null` for in-range
    // (default operator path, no audit flag); a non-null value
    // travels into the backend's `InvoiceDraftCreated` audit payload
    // verbatim. The composer does NOT re-classify here — the SPA's
    // Svelte component owns the operator's confirm UX and writes the
    // discriminant only after the operator has confirmed the out-of-
    // range choice. The backend independently re-classifies via
    // `aberp_billing::classify_delivery_date` for defence in depth.
    deliveryDateOverride: form.deliveryDateOverride,
  };
}

/** PR-82 — trim + normalise a form-supplied note string to `null`
 * when blank, `string` otherwise. Centralised so the per-line and
 * per-invoice note channels share one rule (empty-after-trim ⇒
 * `null`). The backend's note channel is `Option<String>`; passing
 * `Some("")` would be wire-confusing and litter the DuckDB column
 * with empty strings that the renderer would then filter out anyway. */
function blankToNull(raw: string | null | undefined): string | null {
  if (raw === null || raw === undefined) return null;
  const trimmed = raw.trim();
  return trimmed === "" ? null : trimmed;
}

/** PR-77 / session-101 — build the customer-address body shape from
 * the form's four address fields. Returns `undefined` (omitting the
 * wire field) when every field is blank-after-trim so the backend's
 * preflight emits the cleaner `CustomerAddressMissing` message rather
 * than rejecting a body with four empty strings. Otherwise returns
 * the trimmed quartet verbatim — partially-blank shapes flow through
 * because the per-field preflight gate names the precise gap, not a
 * generic "address is malformed" lump. */
export function composeCustomerAddress(
  form: IssueInvoiceFormState,
): { countryCode: string; postalCode: string; city: string; street: string } | undefined {
  const countryCode = form.customerCountryCode.trim();
  const postalCode = form.customerPostalCode.trim();
  const city = form.customerCity.trim();
  const street = form.customerStreet.trim();
  if (
    countryCode === "" &&
    postalCode === "" &&
    city === "" &&
    street === ""
  ) {
    return undefined;
  }
  return { countryCode, postalCode, city, street };
}

/** PR-75 / session-99 — inputs to the Submit-button gate for the
 * bank-picker branch. Pure data; no Svelte runes — so vitest can pin
 * the gate decision without mounting `IssueInvoice.svelte`. */
export interface IssueSubmitGateInputs {
  /** `true` once `loadSellerBanks()` has resolved (success OR caught
   * failure). `false` while the request is in flight. */
  sellerBanksLoaded: boolean;
  /** Non-null when `loadSellerBanks()` rejected. The error message the
   * SPA surfaces inline; presence alone is the gate signal. */
  sellerBanksLoadError: string | null;
  /** Number of bank entries whose currency matches the form's currency.
   * Zero means "no bank account configured for this currency" — the
   * issuance path cannot complete without one. */
  banksForCurrencyCount: number;
}

/** PR-75 / session-99 — closes the live-test regression Ervin caught:
 * clicking "Issue invoice" when no bank entry exists for the form's
 * currency fired a silent request that produced no inline feedback
 * (the backend's bank resolver loud-failed, but the SPA route had no
 * affordance for the bank-missing class of error). Pre-PR-75 the
 * button was always enabled.
 *
 * Returns `true` iff the bank picker is unresolvable; the Svelte
 * component then disables `<button type="submit">` so the operator
 * sees a clearer dead-end (the "no bank for currency" hint above the
 * button + the disabled state) instead of a click that does nothing.
 *
 * Three failure modes, any one of which gates the button:
 *   1. Banks haven't loaded yet (`!sellerBanksLoaded`).
 *   2. Banks load FAILED (`sellerBanksLoadError !== null`).
 *   3. Banks loaded but there are zero entries for the form's current
 *      currency (`banksForCurrencyCount === 0`). */
export function cannotIssueDueToBank(args: IssueSubmitGateInputs): boolean {
  return (
    !args.sellerBanksLoaded ||
    args.sellerBanksLoadError !== null ||
    args.banksForCurrencyCount === 0
  );
}
