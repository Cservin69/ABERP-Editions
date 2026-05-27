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
  composeCustomerAddress,
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
    // PR-82 — inherit any per-line note recorded on the base's
    // side-stored issuance input. The modification form keeps the
    // operator's freedom to edit; the textarea pre-fills with the
    // base value (empty string for unannotated lines).
    note: l.note ?? "",
  }));
  return {
    customerTaxNumber: input.customer.taxNumber,
    customerName: input.customer.name,
    // PR-77 / session-101 — inherit the customer-address quartet from
    // the base's side-stored issuance input. Bases issued pre-PR-77
    // have `customer.address === undefined`; the form fields seed to
    // empty strings and the preflight will fire
    // `CustomerAddressMissing` on submit — recovery is to fill the
    // address by hand or enrich the partner record first.
    customerCountryCode: input.customer.address?.countryCode ?? "",
    customerPostalCode: input.customer.address?.postalCode ?? "",
    customerCity: input.customer.address?.city ?? "",
    customerStreet: input.customer.address?.street ?? "",
    currency: baseCurrency,
    lines,
    modificationDate: todayIsoDate(),
    // PR-73 / ADR-0040 §addendum — modification form inherits the
    // bank-account snapshot from the base implicitly (the backend's
    // `issue_modification` reads the base's snapshot inside the
    // chain-tx and stamps it onto the modification's invoice row).
    // Sending `null` keeps the wire shape clean; the backend ignores
    // the wire field for modifications since inheritance is the rule.
    bankAccountId: null,
    // PR-82 — modification form inherits any invoice-level note from
    // the base's side-stored issuance input. Operator can edit.
    invoiceNote: input.invoiceNote ?? "",
  };
}

/** PR-47β — turn the modification form state into the wire
 * [`ModificationInvoiceRequest`]. Pure function; no side effects.
 * The trim discipline matches `composeIssueInvoiceBody` so the
 * backend's `validate_*` checks see the same trimmed values. */
export function composeModificationBody(
  form: ModificationFormState,
): ModificationInvoiceRequest {
  // PR-77 / session-101 — reuse `composeCustomerAddress` from
  // `issue-invoice.ts` so the address compose discipline is identical
  // across the issue and modification surfaces. The
  // `ModificationFormState extends IssueInvoiceFormState` shape makes
  // the call a direct delegation.
  return {
    customer: {
      taxNumber: form.customerTaxNumber.trim(),
      name: form.customerName.trim(),
      address: composeCustomerAddress(form),
    },
    lines: form.lines.map((l) => ({
      description: l.description.trim(),
      quantity: l.quantity,
      unitPrice: l.unitPriceMinor,
      vatRatePercent: l.vatRatePercent,
      // PR-82 — pass through any per-line note. The backend's
      // modification route does not yet wire chain-level note
      // editing into the audit payload (named-deferred until an
      // operational need surfaces); the persisted line shape still
      // carries the note via the standard allocator path.
      note: l.note.trim() === "" ? null : l.note.trim(),
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
