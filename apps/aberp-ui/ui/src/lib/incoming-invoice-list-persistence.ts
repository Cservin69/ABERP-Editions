// PR-179 / session-179 — persist the IncomingInvoiceList's sort
// selection + status / currency filter to localStorage. Independent of
// the AR list's prefs (separate key) so the operator can keep
// orthogonal views on the two surfaces.
//
// Closed-vocab discipline mirrors `invoice-list-persistence.ts`: a
// persisted column / direction / facet that is no longer in the
// vocab is DISCARDED, never coerced (CLAUDE.md rule 7).

import type { Currency } from "./api";

export type IncomingSortKey =
  | "supplier_name"
  | "supplier_tax_number"
  | "nav_invoice_number"
  | "issue_date"
  | "total_gross"
  | "local_status";

export type SortDir = "asc" | "desc";

export type IncomingStatusFacet = "All" | "Outstanding" | "Paid" | "Irrelevant";

export type IncomingCurrencyFacet = "All" | Currency;

export interface IncomingFilterSpec {
  needle: string;
  status: IncomingStatusFacet;
  currency: IncomingCurrencyFacet;
}

export interface IncomingListPrefs {
  sort: { key: IncomingSortKey | null; dir: SortDir };
  filter: IncomingFilterSpec;
}

export const INCOMING_LIST_PREFS_KEY = "aberp:incoming-invoice-list:prefs";

export const EMPTY_INCOMING_FILTER: IncomingFilterSpec = {
  needle: "",
  status: "All",
  currency: "All",
};

export const DEFAULT_INCOMING_LIST_PREFS: IncomingListPrefs = {
  sort: { key: null, dir: "asc" },
  filter: { ...EMPTY_INCOMING_FILTER },
};

const LEGAL_SORT_KEYS: readonly IncomingSortKey[] = [
  "supplier_name",
  "supplier_tax_number",
  "nav_invoice_number",
  "issue_date",
  "total_gross",
  "local_status",
];

const LEGAL_SORT_DIRS: readonly SortDir[] = ["asc", "desc"];

const LEGAL_STATUSES: readonly IncomingStatusFacet[] = [
  "All",
  "Outstanding",
  "Paid",
  "Irrelevant",
];

const LEGAL_CURRENCIES: readonly IncomingCurrencyFacet[] = ["All", "HUF", "EUR"];

export function loadIncomingListPrefs(
  storage: Pick<Storage, "getItem"> | null = localStorageOrNull(),
): IncomingListPrefs {
  if (storage === null) return cloneDefault();
  let raw: string | null;
  try {
    raw = storage.getItem(INCOMING_LIST_PREFS_KEY);
  } catch (_e) {
    return cloneDefault();
  }
  if (raw === null) return cloneDefault();
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (_e) {
    return cloneDefault();
  }
  return validatePrefs(parsed);
}

export function saveIncomingListPrefs(
  prefs: IncomingListPrefs,
  storage: Pick<Storage, "setItem"> | null = localStorageOrNull(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(INCOMING_LIST_PREFS_KEY, JSON.stringify(prefs));
  } catch (e) {
    // eslint-disable-next-line no-console
    console.warn("aberp: failed to persist incoming invoice list prefs", e);
  }
}

function cloneDefault(): IncomingListPrefs {
  return {
    sort: { ...DEFAULT_INCOMING_LIST_PREFS.sort },
    filter: { ...DEFAULT_INCOMING_LIST_PREFS.filter },
  };
}

function validatePrefs(parsed: unknown): IncomingListPrefs {
  if (parsed === null || typeof parsed !== "object") return cloneDefault();
  const obj = parsed as Record<string, unknown>;
  return {
    sort: validateSort(obj.sort),
    filter: validateFilter(obj.filter),
  };
}

function validateSort(raw: unknown): IncomingListPrefs["sort"] {
  if (raw === null || typeof raw !== "object") {
    return { ...DEFAULT_INCOMING_LIST_PREFS.sort };
  }
  const obj = raw as Record<string, unknown>;
  const dir = LEGAL_SORT_DIRS.includes(obj.dir as SortDir)
    ? (obj.dir as SortDir)
    : "asc";
  if (obj.key === null) return { key: null, dir };
  if (typeof obj.key === "string" && LEGAL_SORT_KEYS.includes(obj.key as IncomingSortKey)) {
    return { key: obj.key as IncomingSortKey, dir };
  }
  return { ...DEFAULT_INCOMING_LIST_PREFS.sort };
}

function validateFilter(raw: unknown): IncomingFilterSpec {
  if (raw === null || typeof raw !== "object") return { ...EMPTY_INCOMING_FILTER };
  const obj = raw as Record<string, unknown>;
  const needle = typeof obj.needle === "string" ? obj.needle : "";
  const status = LEGAL_STATUSES.includes(obj.status as IncomingStatusFacet)
    ? (obj.status as IncomingStatusFacet)
    : "All";
  const currency = LEGAL_CURRENCIES.includes(obj.currency as IncomingCurrencyFacet)
    ? (obj.currency as IncomingCurrencyFacet)
    : "All";
  return { needle, status, currency };
}

function localStorageOrNull(): Storage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage ?? null;
  } catch (_e) {
    return null;
  }
}
