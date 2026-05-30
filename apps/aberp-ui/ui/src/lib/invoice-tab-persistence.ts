// PR-179 / session-179 — persist the Outgoing/Incoming tab selection on
// the Invoices page to `localStorage` so the operator's tab choice
// survives a reload. Two-value closed vocab, same posture as
// `invoice-list-persistence.ts` from S175 (storage-injectable so the
// vitest pin doesn't touch real `window.localStorage`).
//
// Default on first launch is `outgoing` per the SPA-layout brief: the
// AR side is the daily driver, the AP side is the secondary surface.

export type InvoiceTab = "outgoing" | "incoming";

export const INVOICE_TAB_KEY = "aberp:invoice-tab";

export const DEFAULT_INVOICE_TAB: InvoiceTab = "outgoing";

const LEGAL_TABS: readonly InvoiceTab[] = ["outgoing", "incoming"];

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
