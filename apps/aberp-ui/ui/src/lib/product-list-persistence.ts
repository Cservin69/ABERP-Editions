// PR-181 / session-181 — persist the ProductsList's quick-filter
// needle to `localStorage`. Mirrors `partner-list-persistence.ts`
// exactly — separate key per the brief (different vocabs once sort
// + unit/currency facets land, easier to reason about as siblings).
//
// Scope note (CLAUDE.md rule 3 — surgical changes):
// ProductsList today has ONLY a needle search input — no sortable
// column headers, no unit-of-measure / currency facet chips. Per the
// session-181 brief, this PR does NOT introduce sortability or new
// facets (out-of-scope UI expansion). The persisted shape carries
// `filter` alone; when sort columns / facets land later, this helper
// extends additively.
//
// Pinned by `product-list-persistence.test.ts`.

export const PRODUCT_LIST_PREFS_KEY = "aberp:product-list:prefs";

export interface ProductListPrefs {
  filter: { needle: string };
}

export const DEFAULT_PRODUCT_LIST_PREFS: ProductListPrefs = {
  filter: { needle: "" },
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
  return { filter: { ...DEFAULT_PRODUCT_LIST_PREFS.filter } };
}

function validatePrefs(parsed: unknown): ProductListPrefs {
  if (parsed === null || typeof parsed !== "object") return cloneDefault();
  const obj = parsed as Record<string, unknown>;
  return { filter: validateFilter(obj.filter) };
}

function validateFilter(raw: unknown): ProductListPrefs["filter"] {
  if (raw === null || typeof raw !== "object") {
    return { ...DEFAULT_PRODUCT_LIST_PREFS.filter };
  }
  const obj = raw as Record<string, unknown>;
  const needle = typeof obj.needle === "string" ? obj.needle : "";
  return { needle };
}

function localStorageOrNull(): Storage | null {
  try {
    if (typeof window === "undefined") return null;
    return window.localStorage ?? null;
  } catch (_e) {
    return null;
  }
}
