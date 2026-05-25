// PR-47β / session-65 — form-state + form-to-request-body composer
// for the SPA's Modification (Amend invoice) form. Mirrors
// `issue-invoice.ts`'s shape and lives in a pure module so the
// composer is testable under vitest without mounting a Svelte
// component.
//
// The modification form differs from the IssueInvoice form in three
// places per ADR-0024 §1, §3, §4:
//
//   1. Currency is LOCKED to the base invoice's currency (ADR-0037 §4
//      invariant C6 — chain children inherit). The form initialises
//      the field from the base's `detail.currency` and the dropdown
//      renders DISABLED so the operator cannot change it.
//
//   2. A `modificationDate` field carries the operator-supplied
//      `YYYY-MM-DD` per ADR-0024 §1 (frozen on the
//      `InvoiceModificationIssued` audit payload; no silent today-
//      default).
//
//   3. The form fields can be pre-filled from the base's side-stored
//      `InvoiceInputJson` (PR-47α / A174) via
//      [`formFromIssuanceInput`] so the operator edits in place
//      rather than retyping the entire invoice. For CLI-issued
//      invoices the side-store is absent; the form falls back to
//      [`emptyModificationForm`] with the base's currency locked.
//
// Pinned by `modification.test.ts` per the A156 / A161 mirror-
// invariant precedent.

import type {
  Currency,
  IssueInvoiceRequest,
  ModificationInvoiceRequest,
} from "./api";
import {
  emptyForm as emptyIssueForm,
  type IssueInvoiceFormState,
  type LineFormState,
} from "./issue-invoice";

/** PR-47β — modification form state. Same shape as
 * [`IssueInvoiceFormState`] plus the `modificationDate` field per
 * ADR-0024 §1. The `currency` slot stays inherited from the base
 * invoice and the form renders the dropdown DISABLED — but the
 * shape itself is identical so the composers can share their inner
 * mapping logic. */
export interface ModificationFormState extends IssueInvoiceFormState {
  /** ADR-0024 §1 — operator-supplied `YYYY-MM-DD`. Frozen on the
   * `InvoiceModificationIssued` audit payload at issuance time. */
  modificationDate: string;
}

/** PR-47β — sensible defaults for an empty modification form when
 * pre-fill is unavailable (CLI-issued base; pre-PR-47α SPA-issued
 * base). The operator types fresh values, but `currency` is locked to
 * the base's currency per C6 — the caller passes it in rather than
 * defaulting to HUF. `modificationDate` defaults to today (browser
 * local date) — the operator is free to overwrite, but ADR-0024 §1's
 * no-silent-default is preserved at the BACKEND boundary; the form
 * surfaces a sensible starting value while the backend still
 * validates the canonical YYYY-MM-DD shape. */
export function emptyModificationForm(
  baseCurrency: Currency,
): ModificationFormState {
  const base = emptyIssueForm();
  return {
    ...base,
    currency: baseCurrency,
    modificationDate: todayIsoDate(),
  };
}

/** PR-47β — pre-fill a modification form from the operator's
 * original [`IssueInvoiceRequest`]-shaped issuance input (returned by
 * `GET /api/invoices/<id>/issuance-input` per A174). The currency
 * field is locked to the base's currency (passed in separately rather
 * than read from the body so the C6 invariant is sourced from the
 * billing row, not the operator-edited body). `modificationDate`
 * defaults to today; the operator can overwrite. */
export function formFromIssuanceInput(
  input: IssueInvoiceRequest,
  baseCurrency: Currency,
): ModificationFormState {
  const lines: LineFormState[] = input.lines.map((l) => ({
    description: l.description,
    quantity: l.quantity,
    unitPriceMinor: l.unitPrice,
    vatRatePercent: l.vatRatePercent,
  }));
  return {
    supplierTaxNumber: input.supplier.taxNumber,
    supplierName: input.supplier.name,
    supplierCountryCode: input.supplier.address.countryCode,
    supplierPostalCode: input.supplier.address.postalCode,
    supplierCity: input.supplier.address.city,
    supplierStreet: input.supplier.address.street,
    customerTaxNumber: input.customer.taxNumber,
    customerName: input.customer.name,
    currency: baseCurrency,
    lines,
    modificationDate: todayIsoDate(),
  };
}

/** PR-47β — turn the modification form state into the wire
 * [`ModificationInvoiceRequest`]. Pure function; no side effects.
 * The trim discipline matches `composeIssueInvoiceBody` so the
 * backend's `validate_*` checks see the same trimmed values. */
export function composeModificationBody(
  form: ModificationFormState,
): ModificationInvoiceRequest {
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
    modificationDate: form.modificationDate.trim(),
  };
}

/** Browser-local today as `YYYY-MM-DD`. Used as the default
 * starting value for `modificationDate`; the operator can overwrite. */
function todayIsoDate(): string {
  const d = new Date();
  const yyyy = d.getFullYear().toString().padStart(4, "0");
  const mm = (d.getMonth() + 1).toString().padStart(2, "0");
  const dd = d.getDate().toString().padStart(2, "0");
  return `${yyyy}-${mm}-${dd}`;
}
