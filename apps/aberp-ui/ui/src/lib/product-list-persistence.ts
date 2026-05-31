// PR-181 / session-181 — persist the ProductsList's quick-filter
// needle to `localStorage`. PR-194 / session-194 — extend with sort
// + Unit + Currency facets (parity with InvoiceList S119 / S175).
// Backward-compat: a legacy needle-only blob loads cleanly; missing
// sort + facets fall back to defaults.
//
// Unit-facet validation note: the closed vocab is NOT compile-time
// fixed (the operator can create products with arbitrary `Own:<text>`
// labels). The persistence validator accepts any non-empty string
// here; the component-level renderer cross-checks against the
// currently-loaded rows and resets to `"All"` if the persisted unit
// is not present (otherwise the operator sees an inactive filter
// they can't clear by inspecting the dropdown).
//
// Pinned by `product-list-persistence.test.ts`.

import {
  LEGAL_CURRENCY_FACETS,
  LEGAL_PRODUCT_SORT_KEYS,
  type CurrencyFacet,
  type ProductFilterSpec,
  type ProductSortKey,
  type UnitFacet,
} from "./products";
import type { SortDir } from "./list-sort";

export const PRODUCT_LIST_PREFS_KEY = "aberp:product-list:prefs";

const LEGAL_SORT_DIRS: readonly SortDir[] = ["asc", "desc"];

export interface ProductListPrefs {
  sort: { key: ProductSortKey | null; dir: SortDir };
  filter: ProductFilterSpec;
}

export const DEFAULT_PRODUCT_LIST_PREFS: ProductListPrefs = {
  sort: { key: null, dir: "asc" },
  filter: { needle: "", unit: "All", currency: "All" },
};

export function loadProductListPrefs(
  storage: Pick<Storage, "getItem"> | null = localStorageOrNull(),
): ProductListPrefs {
  if (storage === null) return cloneDefault();
  let raw: string | null;
  try {
    raw = storage.getItem(PRODUCT_LIST_PREFS_KEY);
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

export function saveProductListPrefs(
  prefs: ProductListPrefs,
  storage: Pick<Storage, "setItem"> | null = localStorageOrNull(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(PRODUCT_LIST_PREFS_KEY, JSON.stringify(prefs));
  } catch (e) {
    // eslint-disable-next-line no-console
    console.warn("aberp: failed to persist product list prefs", e);
  }
}

function cloneDefault(): ProductListPrefs {
  return {
    sort: { ...DEFAULT_PRODUCT_LIST_PREFS.sort },
    filter: { ...DEFAULT_PRODUCT_LIST_PREFS.filter },
  };
}

function validatePrefs(parsed: unknown): ProductListPrefs {
  if (parsed === null || typeof parsed !== "object") return cloneDefault();
  const obj = parsed as Record<string, unknown>;
  return {
    sort: validateSort(obj.sort),
    filter: validateFilter(obj.filter),
  };
}

function validateSort(raw: unknown): ProductListPrefs["sort"] {
  if (raw === null || typeof raw !== "object") {
    return { ...DEFAULT_PRODUCT_LIST_PREFS.sort };
  }
  const obj = raw as Record<string, unknown>;
  const dir = LEGAL_SORT_DIRS.includes(obj.dir as SortDir)
    ? (obj.dir as SortDir)
    : "asc";
  if (obj.key === null) return { key: null, dir };
  if (
    typeof obj.key === "string" &&
    LEGAL_PRODUCT_SORT_KEYS.includes(obj.key as ProductSortKey)
  ) {
    return { key: obj.key as ProductSortKey, dir };
  }
  return { ...DEFAULT_PRODUCT_LIST_PREFS.sort };
}

function validateFilter(raw: unknown): ProductFilterSpec {
  if (raw === null || typeof raw !== "object") {
    return { ...DEFAULT_PRODUCT_LIST_PREFS.filter };
  }
  const obj = raw as Record<string, unknown>;
  const needle = typeof obj.needle === "string" ? obj.needle : "";
  const unit = validateUnitFacet(obj.unit);
  const currency = validateCurrencyFacet(obj.currency);
  return { needle, unit, currency };
}

function validateUnitFacet(raw: unknown): UnitFacet {
  // The Unit facet vocab is open-ended (the `Own:<label>` branch lets
  // the operator coin arbitrary labels). Accept any non-empty string
  // here; the component-level renderer resets to `"All"` if the
  // persisted value matches no current row.
  if (raw === "All") return "All";
  if (typeof raw === "string" && raw.length > 0) return raw;
  return "All";
}

function validateCurrencyFacet(raw: unknown): CurrencyFacet {
  if (
    typeof raw === "string" &&
    LEGAL_CURRENCY_FACETS.includes(raw as CurrencyFacet)
  ) {
    return raw as CurrencyFacet;
  }
  return "All";
}

function localStorageOrNull(): Storage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage ?? null;
  } catch (_e) {
    return null;
  }
}
