// PR-175 / session-175 — persist the invoice list's sort selection
// + quick-filter facets to `localStorage` so the operator's view
// survives a page reload / app restart. Pure helpers; the Svelte
// component only calls `loadInvoiceListPrefs` on mount and
// `saveInvoiceListPrefs` on every state mutation.
//
// The SPA is single-tenant per running process (the backend swaps
// `~/.aberp/<tenant>/seller.toml` out of band; the SPA itself is
// tenant-blind at this layer), so one global key is correct today.
// If a future change exposes tenant selection at the SPA layer,
// prefix the key with the tenant — same pattern, additive change.
//
// Pinned by `invoice-list-persistence.test.ts`.
//
// Closed-vocab discipline (CLAUDE.md rule 7 — surface conflicts,
// don't average them): a persisted column key that's not in the
// current `SortKey` vocab (renamed / removed in a later PR) is
// discarded, not coerced. Same for direction, state facet, and
// currency facet. Stale data falls back to defaults; the operator's
// first sort click overwrites it cleanly.

import type { Currency, InvoiceState } from "./api";
import { LIFECYCLE_ORDER } from "./labels";
import { EMPTY_FILTER, type InvoiceFilterSpec, type SortDir, type SortKey } from "./invoice-list";

/** Storage key. Namespaced under `aberp:` per the pre-existing
 * `aberp:just-issued-invoice-id` sessionStorage convention from
 * PR-87 / session-112. */
export const INVOICE_LIST_PREFS_KEY = "aberp:invoice-list:prefs";

/** Closed-vocab of legal sort keys. Mirrors `SortKey` in
 * `invoice-list.ts`. Kept as a runtime list so the load path can
 * validate a persisted key against it without a TypeScript-only
 * guard (the persisted JSON is untyped at runtime). */
const LEGAL_SORT_KEYS: readonly SortKey[] = [
  "invoice_id",
  "invoice_number",
  "partner",
  "series_number",
  "fiscal_year",
  "state",
  "total",
];

const LEGAL_SORT_DIRS: readonly SortDir[] = ["asc", "desc"];

const LEGAL_CURRENCIES: readonly Currency[] = ["HUF", "EUR"];

/** Persisted shape. `sort.key === null` is the lifecycle-natural
 * fallback the Svelte component already documents as the default;
 * persisting `null` is legal so an operator who three-cycle-resets
 * a sort retains the reset across reload (CLAUDE.md rule 12 — the
 * persisted view matches what they saw last). */
export interface InvoiceListPrefs {
  sort: { key: SortKey | null; dir: SortDir };
  filter: InvoiceFilterSpec;
}

/** Default prefs: no sort + open filter. Returned by `loadInvoiceListPrefs`
 * when nothing is persisted yet, or when the persisted blob is
 * malformed / contains unknown vocab. */
export const DEFAULT_INVOICE_LIST_PREFS: InvoiceListPrefs = {
  sort: { key: null, dir: "asc" },
  filter: { ...EMPTY_FILTER },
};

/** Read the persisted prefs from `localStorage`. Returns the default
 * blob on any failure path:
 *   - key absent (fresh install)
 *   - JSON.parse throws (corrupted blob)
 *   - shape mismatch (legacy / future schema)
 *   - column / direction / facet contains unknown vocab (stale key
 *     renamed in a later PR; see CLAUDE.md rule 7 — discard, don't
 *     coerce)
 *   - `localStorage` itself unavailable (private browsing, quota
 *     exceeded, SSR context)
 *
 * The helper is intentionally storage-injectable so the vitest pin
 * doesn't touch the real `window.localStorage` (the SPA's vitest
 * setup has no jsdom layer; the injected stub mirrors the read
 * surface). Production callers pass nothing and get the default
 * `localStorageOrNull()` lookup. */
export function loadInvoiceListPrefs(
  storage: Pick<Storage, "getItem"> | null = localStorageOrNull(),
): InvoiceListPrefs {
  if (storage === null) return cloneDefault();
  let raw: string | null;
  try {
    raw = storage.getItem(INVOICE_LIST_PREFS_KEY);
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

/** Write the prefs blob to `localStorage`. Fire-and-forget per the
 * task brief: a throw from `setItem` (private browsing, quota
 * exceeded) surfaces as a `console.warn` so a regression that
 * silently drops every save is visible in the devtools console,
 * without breaking the operator's interaction. */
export function saveInvoiceListPrefs(
  prefs: InvoiceListPrefs,
  storage: Pick<Storage, "setItem"> | null = localStorageOrNull(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(INVOICE_LIST_PREFS_KEY, JSON.stringify(prefs));
  } catch (e) {
    // eslint-disable-next-line no-console
    console.warn("aberp: failed to persist invoice list prefs", e);
  }
}

function cloneDefault(): InvoiceListPrefs {
  return {
    sort: { ...DEFAULT_INVOICE_LIST_PREFS.sort },
    filter: { ...DEFAULT_INVOICE_LIST_PREFS.filter },
  };
}

/** Validate a parsed blob against the current closed-vocab. Returns
 * a fresh prefs object — never mutates the input — so a partial
 * shape (e.g. legacy blob with only `sort`, no `filter`) falls back
 * cleanly without leaking undefined into the Svelte `$state`. */
function validatePrefs(parsed: unknown): InvoiceListPrefs {
  if (parsed === null || typeof parsed !== "object") return cloneDefault();
  const obj = parsed as Record<string, unknown>;
  return {
    sort: validateSort(obj.sort),
    filter: validateFilter(obj.filter),
  };
}

function validateSort(raw: unknown): InvoiceListPrefs["sort"] {
  if (raw === null || typeof raw !== "object") {
    return { ...DEFAULT_INVOICE_LIST_PREFS.sort };
  }
  const obj = raw as Record<string, unknown>;
  const dir = LEGAL_SORT_DIRS.includes(obj.dir as SortDir)
    ? (obj.dir as SortDir)
    : "asc";
  if (obj.key === null) return { key: null, dir };
  if (typeof obj.key === "string" && LEGAL_SORT_KEYS.includes(obj.key as SortKey)) {
    return { key: obj.key as SortKey, dir };
  }
  // Unknown / missing key — fall back to the lifecycle-natural
  // default. The dir is discarded with the key (an "asc on nothing"
  // is meaningless; the next sort click sets both fresh).
  return { ...DEFAULT_INVOICE_LIST_PREFS.sort };
}

function validateFilter(raw: unknown): InvoiceFilterSpec {
  if (raw === null || typeof raw !== "object") {
    return { ...EMPTY_FILTER };
  }
  const obj = raw as Record<string, unknown>;
  const needle = typeof obj.needle === "string" ? obj.needle : "";
  const state = validateStateFacet(obj.state);
  const currency = validateCurrencyFacet(obj.currency);
  return { needle, state, currency };
}

function validateStateFacet(raw: unknown): InvoiceFilterSpec["state"] {
  if (raw === "All") return "All";
  if (typeof raw === "string" && (LIFECYCLE_ORDER as readonly string[]).includes(raw)) {
    return raw as InvoiceState;
  }
  return "All";
}

function validateCurrencyFacet(raw: unknown): InvoiceFilterSpec["currency"] {
  if (raw === "All") return "All";
  if (typeof raw === "string" && LEGAL_CURRENCIES.includes(raw as Currency)) {
    return raw as Currency;
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
