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

/** PR-44ζ — per-line form state. `unitPriceMinor` is the operator-
 * typed amount: whole forints for HUF, cents for EUR (the SPA mirrors
 * the issuance-path posture documented on
 * `InvoiceListItem.total_gross`). `quantity` and `vatRatePercent`
 * are integers. */
export interface LineFormState {
  description: string;
  quantity: number;
  unitPriceMinor: number;
  vatRatePercent: number;
}

/** PR-44ζ — top-level form state. Captures every operator-typed
 * value the form exposes; the composer reshapes it into the wire
 * `IssueInvoiceRequest`. */
export interface IssueInvoiceFormState {
  supplierTaxNumber: string;
  supplierName: string;
  supplierCountryCode: string;
  supplierPostalCode: string;
  supplierCity: string;
  supplierStreet: string;
  customerTaxNumber: string;
  customerName: string;
  currency: Currency;
  lines: LineFormState[];
}

/** PR-44ζ — sensible defaults for an empty form. The 27% VAT rate is
 * the Hungarian standard rate; HUF is the default currency (matches
 * the CLI's default). One empty line is included so the form is
 * editable on first paint without a separate "+ Add line" click. */
export function emptyForm(): IssueInvoiceFormState {
  return {
    supplierTaxNumber: "",
    supplierName: "",
    supplierCountryCode: "HU",
    supplierPostalCode: "",
    supplierCity: "",
    supplierStreet: "",
    customerTaxNumber: "",
    customerName: "",
    currency: "HUF",
    lines: [emptyLine()],
  };
}

/** PR-44ζ — sensible defaults for a freshly-added line. */
export function emptyLine(): LineFormState {
  return {
    description: "",
    quantity: 1,
    unitPriceMinor: 0,
    vatRatePercent: 27,
  };
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
    supplier: {
      taxNumber: form.supplierTaxNumber.trim(),
      name: form.supplierName.trim(),
      address: {
        countryCode: form.supplierCountryCode.trim(),
        postalCode: form.supplierPostalCode.trim(),
        city: form.supplierCity.trim(),
        street: form.supplierStreet.trim(),
      },
    },
    customer: {
      taxNumber: form.customerTaxNumber.trim(),
      name: form.customerName.trim(),
    },
    lines: form.lines.map((l) => ({
      description: l.description.trim(),
      quantity: l.quantity,
      unitPrice: l.unitPriceMinor,
      vatRatePercent: l.vatRatePercent,
    })),
    currency: form.currency,
  };
}
