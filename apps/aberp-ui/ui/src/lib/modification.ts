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
import { formatMinorToInput, parseAmountToMinor, parseDecimalQuantity } from "./format";
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
    // S157 — pre-fill the editable quantity string from the base's
    // stored value. `String(...)` tolerates BOTH the new string wire
    // shape (`"1.5"`) and pre-S157 side-store rows that carry a JSON
    // number (`1`); `parseDecimalQuantity` re-parses it on submit.
    quantityInput: String(l.quantity),
    // PR-88 / session-113 — convert the backend's integer minor-unit
    // count back into the operator-editable display string the form
    // pre-fills with. For HUF (0-decimal) this is the bare integer;
    // for EUR (2-decimal) it's `"125.00"` form. The parser round-
    // trips both back to the same minor count on submit.
    unitPriceInput: formatMinorToInput(l.unitPrice, baseCurrency),
    vatRatePercent: l.vatRatePercent,
    // ADR-0101 — inherit the base's per-line VAT rate-kind from the
    // side-stored issuance input (default `"Percent"` for pre-0101 bases,
    // mirroring the backend serde default). This keeps the shared
    // `LineFormState` type-complete. NOTE (FLAG for adversarial review):
    // the modification SPA does NOT yet expose a VAT-kind selector and
    // `ModificationInvoiceRequest` carries NO `vatRateKind` field, so a
    // modification of a (future) NEW-KIND invoice would not thread the kind
    // through the modification wire path — a named-deferred follow-up. There
    // is ZERO current exposure: no non-`Percent` invoice can exist until this
    // session opens the issue door, so every real base inherits `"Percent"`.
    vatRateKind: l.vatRateKind ?? "Percent",
    // PR-82 — inherit any per-line note recorded on the base's
    // side-stored issuance input. The modification form keeps the
    // operator's freedom to edit; the textarea pre-fills with the
    // base value (empty string for unannotated lines).
    note: l.note ?? "",
  }));
  return {
    // PR-97 / ADR-0048 — inherit the base invoice's buyer-kind
    // discriminator from the side-stored input.json. Pre-PR-97 bases
    // omit the field and the JSON layer's `vatStatus ?? "Domestic"`
    // mirrors the backend serde default — keeps chain operations on
    // pre-PR-97 bases emitting Domestic wire bodies.
    customerVatStatus: input.customer.vatStatus ?? "Domestic",
    // PR-97 / ADR-0048 (Ervin override 1) — modification path does
    // NOT increment `partners.issued_invoice_count` (the base's
    // counter was already booked at issue time). Carry the
    // partner_id forward through the form state for type-compatibility
    // with `IssueInvoiceFormState`; the modification's
    // `composeModificationBody` does NOT emit it on the wire (the
    // modification wire body has no `partnerId` field — the backend
    // does not increment on chain ops).
    customerPartnerId: input.customer.partnerId ?? null,
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
    // ADR-0102 — inherit the base's EU community VAT number (Other
    // buyers). Pre-0102 bases omit it → empty string. In-app modification
    // of a non-`Percent` (EU-0) base is blocked backend-side, but an
    // Other + Percent base is modifiable, so the number must round-trip.
    customerCommunityVatNumber: input.customer.communityVatNumber ?? "",
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
    // PR-84 — modification form inherits the date fields from the
    // base where present (the side-stored issuance input may carry
    // them post-PR-84) and falls back to `modificationDate` otherwise.
    // The modification UX does not surface the comfort-zone pickers
    // yet (out of scope per the PR-84 brief: "keep PR-84 to the
    // issue path"); these fields are present on the form state for
    // type-compatibility with `IssueInvoiceFormState` but the
    // modification's `composeModificationBody` does not thread them
    // onto the wire.
    invoiceDate: input.deliveryDate ?? todayIsoDate(),
    paymentDeadline: input.paymentDeadline ?? todayIsoDate(),
    deliveryDate: input.deliveryDate ?? todayIsoDate(),
    deliveryDateOverride: null,
    // PR-92 — type-compatibility with IssueInvoiceFormState. The
    // modification's compose path does NOT auto-send (the operator
    // sends the modification PDF manually from its detail page);
    // the field is present so the form state type-checks against
    // the shared IssueInvoiceFormState base.
    emailBuyerOnIssue: false,
    // PR-99 Item 4 Part B — same type-compatibility shim. The
    // modification path does NOT auto-submit; the operator handles
    // the storno/amendment NAV submission manually via the detail
    // surface.
    submitToNavOnIssue: false,
    // S160 / ADR-0050 — type-compatibility with IssueInvoiceFormState.
    // Inherit the base invoice's payment method from its side-stored
    // issuance input (pre-S160 bases default to "TRANSFER"). The
    // modification's `composeModificationBody` does NOT thread it onto
    // the `ModificationInvoiceRequest` wire body (the backend modification
    // route defaults it to Transfer — full SPA-route inheritance is a
    // follow-up per ADR-0050 §Consequences); the field is present so the
    // shared form-state type-checks.
    paymentMethod: input.paymentMethod ?? "TRANSFER",
    // PR-203 / S203 — modification form inherits the per-invoice email
    // recipient override from the base's side-stored issuance input. The
    // operator can edit it for THIS modification (the modification's own
    // override is persisted on its OWN invoice row — base unchanged).
    // Pre-PR-203 bases emit `null` here; the form opens with an empty
    // field and the operator can type one for this send.
    emailRecipientOverride: input.emailRecipientOverride ?? "",
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
      // PR-97 / ADR-0048 — propagate the inherited buyer-kind onto the
      // modification's wire body.
      vatStatus: form.customerVatStatus,
      taxNumber: form.customerTaxNumber.trim(),
      // ADR-0102 — propagate the EU community VAT number for Other buyers.
      communityVatNumber:
        form.customerVatStatus === "Other" &&
        form.customerCommunityVatNumber.trim().length > 0
          ? form.customerCommunityVatNumber.trim()
          : undefined,
      name: form.customerName.trim(),
      address: composeCustomerAddress(form),
    },
    lines: form.lines.map((l) => ({
      description: l.description.trim(),
      // S157 — parse the operator-typed quantity (`1.5`/`1,5`) to the
      // canonical dot-decimal string; failure → `"0"` so the backend
      // preflight surfaces `LineItemQuantityZero`.
      quantity: parseDecimalQuantity(l.quantityInput) ?? "0",
      // PR-88 / session-113 — same parse-at-compose-time discipline as
      // `composeIssueInvoiceBody`. The modification form's unit-price
      // input is the operator-typed string; the parser converts to
      // minor units using the form's (locked) currency. Failure → 0,
      // backend preflight surfaces `LineItemUnitPriceNonPositive`.
      unitPrice: parseAmountToMinor(l.unitPriceInput, form.currency) ?? 0,
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
    // PR-203 / S203 — per-modification email recipient override. Blank-
    // after-trim becomes `null` on the wire so the backend resolver sees
    // the "no override, fall back to partner.email" signal verbatim.
    emailRecipientOverride:
      form.emailRecipientOverride.trim() === ""
        ? null
        : form.emailRecipientOverride.trim(),
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
