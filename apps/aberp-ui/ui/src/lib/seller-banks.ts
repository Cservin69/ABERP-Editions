// PR-72 / session-94 — pure-module helpers for the SPA's Tenant
// Settings → "Bank accounts" subsection + the SellerConfigWizard's
// multi-row bank affordance. Composer + wire-to-form mapper + typed-
// error-body parser live here so vitest can pin them without
// mounting a Svelte component (A156 / A161 / A163 mirror-invariant
// composer-pin pattern; same posture as `partners.ts` and
// `seller-config.ts`).
//
// Pinned by `seller-banks.test.ts`.

import type {
  SellerBankCurrency,
  SellerBankInputs,
  SellerBankResponse,
  SellerBankValidationErrorBody,
} from "./api";

/** PR-72 / session-94 — operator-typed form state for one bank-
 * account row. camelCase per the SPA convention; the composer
 * snake_cases on the way to the wire. `setAsDefault` is only
 * meaningful on the create / edit submit (the dedicated set-default
 * flip route has no body). */
export interface SellerBankFormState {
  currency: SellerBankCurrency;
  accountNumber: string;
  bankName: string;
  swiftBic: string;
  setAsDefault: boolean;
}

/** PR-72 / session-94 — default form for a fresh "Add bank account"
 * modal mount. Currency defaults to HUF (the dominant tenant case is
 * Hungarian-domestic). `setAsDefault` defaults to `false`; the route
 * layer auto-promotes the first entry for an unrepresented currency
 * to default per the brief. */
export function emptySellerBankForm(): SellerBankFormState {
  return {
    currency: "HUF",
    accountNumber: "",
    bankName: "",
    swiftBic: "",
    setAsDefault: false,
  };
}

/** PR-72 / session-94 — fold a fetched [`SellerBankResponse`] into
 * the edit-mode form state. `setAsDefault` is intentionally `false`
 * here — the Edit modal does NOT surface the set-as-default
 * checkbox when the row is already the default (the brief: "only
 * visible on Add, or on Edit when this entry isn't already the
 * default"). The Svelte component reads `bank.is_default` directly
 * for the checkbox visibility. */
export function formFromSellerBank(bank: SellerBankResponse): SellerBankFormState {
  return {
    currency: bank.currency,
    accountNumber: bank.account_number,
    bankName: bank.bank_name,
    swiftBic: bank.swift_bic,
    setAsDefault: false,
  };
}

/** PR-72 / session-94 — turn the form state into the wire request
 * body. Trims every string field so a `"   "` operator value
 * surfaces as the backend's actionable validation error rather than
 * slipping through. SWIFT/BIC is uppercase-normalised on the way to
 * the wire (operator usability — every BIC in practice is uppercase,
 * and the SWIFT-country-code inference at PR-71 load time reads the
 * canonical upper-case form). */
export function composeSellerBankInputs(
  form: SellerBankFormState,
): SellerBankInputs {
  return {
    currency: form.currency,
    account_number: form.accountNumber.trim(),
    bank_name: form.bankName.trim(),
    swift_bic: form.swiftBic.trim().toUpperCase(),
    set_as_default: form.setAsDefault,
  };
}

/** PR-72 / session-94 — client-side validator. Returns one inline-
 * error message per failing field (camelCase keys matching the form
 * field names). The backend re-validates AND surfaces additional
 * invariant errors (duplicate-id, multiple-defaults) the operator
 * can't easily check client-side; backend errors take precedence
 * over client errors in the Svelte component per A157. */
export interface SellerBankValidation {
  accountNumber: string | null;
  bankName: string | null;
  swiftBic: string | null;
  ok: boolean;
}

export function validateSellerBankForm(
  form: SellerBankFormState,
): SellerBankValidation {
  const accountNumber =
    form.accountNumber.trim().length === 0 ? "Account number is required" : null;
  const bankName =
    form.bankName.trim().length === 0 ? "Bank name is required" : null;
  const swiftBic =
    form.swiftBic.trim().length === 0
      ? "SWIFT / BIC is required"
      : null;
  const ok = accountNumber === null && bankName === null && swiftBic === null;
  return { accountNumber, bankName, swiftBic, ok };
}

/** PR-72 / session-94 — parse the typed 400 body for the bank-
 * accounts routes. Mirrors `parsePartnerValidationError` shape so the
 * Svelte component's catch arm folds the per-field messages into the
 * inline-error renderer without a new parser per surface. Returns
 * `null` for any other shape so the caller falls back to a generic
 * raw-string display. */
export function parseSellerBankValidationError(
  raw: string,
): SellerBankValidationErrorBody | null {
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

/** PR-72 / session-94 — group the loaded collection by currency for
 * the Tenant Settings list view. The Svelte template iterates this
 * to render per-currency sections; ordering matches declaration
 * order from the backend (the canonical iteration order pinned in
 * `seller_banks::SellerBanks::entries`). */
export interface SellerBankCurrencyGroup {
  currency: SellerBankCurrency;
  banks: SellerBankResponse[];
}

export function groupSellerBanksByCurrency(
  banks: SellerBankResponse[],
): SellerBankCurrencyGroup[] {
  // Preserve first-appearance order of currencies so the UI is
  // deterministic across re-renders without a hard-coded HUF→EUR
  // ordering rule (a future operator with EUR-first banks shouldn't
  // see them shoved below an empty HUF section).
  const order: SellerBankCurrency[] = [];
  const groups: Map<SellerBankCurrency, SellerBankResponse[]> = new Map();
  for (const bank of banks) {
    if (!groups.has(bank.currency)) {
      groups.set(bank.currency, []);
      order.push(bank.currency);
    }
    groups.get(bank.currency)!.push(bank);
  }
  return order.map((currency) => ({
    currency,
    banks: groups.get(currency)!,
  }));
}

/** PR-72 / session-94 — multi-row form state for the
 * SellerConfigWizard's bank step. Operator starts with one HUF row
 * (set as default); "+ Add another bank account" appends. Submit
 * validates: ≥ 1 row, exactly 1 default per used currency. */
export interface WizardBankRow extends SellerBankFormState {
  /** Stable client-side key for the Svelte `{#each}` block so a
   * mid-list delete does not re-mount sibling rows + lose their
   * input focus. Not sent to the wire. */
  rowKey: string;
}

export function emptyWizardBankRows(): WizardBankRow[] {
  return [
    {
      ...emptySellerBankForm(),
      setAsDefault: true,
      rowKey: "row-1",
    },
  ];
}

/** PR-72 / session-94 — wizard-submit validator. Returns either
 * `{ ok: true, rows }` (ready to POST one-row-at-a-time) or a list of
 * row-keyed errors. Enforces the two PR-A schema invariants:
 *   1. ≥ 1 row total (the operator can't finish setup with zero
 *      banks).
 *   2. Exactly one `setAsDefault = true` per currency that has rows. */
export type WizardBankValidation =
  | { ok: true; rows: WizardBankRow[] }
  | { ok: false; rowErrors: Map<string, SellerBankValidation>; summary: string };

export function validateWizardBankRows(
  rows: WizardBankRow[],
): WizardBankValidation {
  if (rows.length === 0) {
    return {
      ok: false,
      rowErrors: new Map(),
      summary: "Add at least one bank account before continuing.",
    };
  }
  const rowErrors: Map<string, SellerBankValidation> = new Map();
  let anyRowInvalid = false;
  for (const row of rows) {
    const v = validateSellerBankForm(row);
    rowErrors.set(row.rowKey, v);
    if (!v.ok) anyRowInvalid = true;
  }
  if (anyRowInvalid) {
    return {
      ok: false,
      rowErrors,
      summary: "One or more bank rows have missing fields.",
    };
  }
  // Defaults invariant: one default per currency that has rows.
  const defaultsByCurrency: Map<SellerBankCurrency, number> = new Map();
  const rowsByCurrency: Map<SellerBankCurrency, number> = new Map();
  for (const row of rows) {
    rowsByCurrency.set(row.currency, (rowsByCurrency.get(row.currency) ?? 0) + 1);
    if (row.setAsDefault) {
      defaultsByCurrency.set(
        row.currency,
        (defaultsByCurrency.get(row.currency) ?? 0) + 1,
      );
    }
  }
  for (const currency of rowsByCurrency.keys()) {
    const defaults = defaultsByCurrency.get(currency) ?? 0;
    if (defaults === 0) {
      return {
        ok: false,
        rowErrors,
        summary: `Mark exactly one ${currency} bank as the default.`,
      };
    }
    if (defaults > 1) {
      return {
        ok: false,
        rowErrors,
        summary: `Mark exactly one ${currency} bank as the default (currently ${defaults}).`,
      };
    }
  }
  return { ok: true, rows };
}
