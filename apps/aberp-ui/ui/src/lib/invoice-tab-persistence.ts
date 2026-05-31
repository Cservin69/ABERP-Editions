// PR-179 / session-179 — persist the Outgoing/Incoming tab selection on
// the Invoices page to `localStorage`. Storage-injectable so the
// vitest pin doesn't touch real `window.localStorage`.
//
// Default on first launch is `outgoing` per the SPA-layout brief: the
// AR side is the daily driver.
//
// S211 / PR-210 — third value `quotes` added for the quote-intake
// operator queue. Closed-vocab discard pattern from S175 means a
// previously-saved unknown value falls back to the default.

export type InvoiceTab = "outgoing" | "incoming" | "quotes";

export const INVOICE_TAB_KEY = "aberp:invoice-tab";

export const DEFAULT_INVOICE_TAB: InvoiceTab = "outgoing";

const LEGAL_TABS: readonly InvoiceTab[] = ["outgoing", "incoming", "quotes"];

export function loadInvoiceTab(
  storage: Pick<Storage, "getItem"> | null = localStorageOrNull(),
): InvoiceTab {
  if (storage === null) return DEFAULT_INVOICE_TAB;
  let raw: string | null;
  try {
    raw = storage.getItem(INVOICE_TAB_KEY);
  } catch (_e) {
    return DEFAULT_INVOICE_TAB;
  }
  if (raw === null) return DEFAULT_INVOICE_TAB;
  if (LEGAL_TABS.includes(raw as InvoiceTab)) return raw as InvoiceTab;
  return DEFAULT_INVOICE_TAB;
}

export function saveInvoiceTab(
  tab: InvoiceTab,
  storage: Pick<Storage, "setItem"> | null = localStorageOrNull(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(INVOICE_TAB_KEY, tab);
  } catch (e) {
    // eslint-disable-next-line no-console
    console.warn("aberp: failed to persist invoice tab", e);
  }
}

function localStorageOrNull(): Storage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage ?? null;
  } catch (_e) {
    return null;
  }
}
