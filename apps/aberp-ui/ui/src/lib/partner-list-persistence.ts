// PR-181 / session-181 — persist the PartnersList's quick-filter
// needle to `localStorage`. PR-194 / session-194 — extend with sort
// + Kind facet so the list reaches parity with InvoiceList (S119 /
// S175). Backward-compat: a legacy blob persisted by PR-181 (needle
// only) loads cleanly; the missing sort + kind fields fall back to
// defaults silently (the validator-discards-unknown-keys pattern
// from S175 / S181).
//
// Independent of the AR / AP list prefs (separate key) so the
// operator can keep orthogonal views across surfaces. Storage-injectable
// for vitest pins.
//
// Closed-vocab discipline mirrors `invoice-list-persistence.ts`: a
// non-string needle / unknown sort key / unknown direction / unknown
// kind in the persisted blob falls back to the default. CLAUDE.md
// rule 7 — discard stale vocab, don't average it.
//
// Pinned by `partner-list-persistence.test.ts`.

import {
  LEGAL_PARTNER_KIND_FACETS,
  LEGAL_PARTNER_SORT_KEYS,
  type PartnerFilterSpec,
  type PartnerKindFacet,
  type PartnerSortKey,
} from "./partners";
import type { SortDir } from "./list-sort";

/** Storage key. Namespaced under `aberp:` per the convention. */
export const PARTNER_LIST_PREFS_KEY = "aberp:partner-list:prefs";

const LEGAL_SORT_DIRS: readonly SortDir[] = ["asc", "desc"];

/** PR-194 — persisted shape. `sort.key === null` keeps the natural
 * (backend-ordered) display the screen has shipped with since PR-54
 * (`ORDER BY display_name ASC`); persisting `null` is legal so an
 * operator who three-cycle-resets a sort retains the reset across
 * reload. */
export interface PartnerListPrefs {
  sort: { key: PartnerSortKey | null; dir: SortDir };
  filter: PartnerFilterSpec;
}

export const DEFAULT_PARTNER_LIST_PREFS: PartnerListPrefs = {
  sort: { key: null, dir: "asc" },
  filter: { needle: "", kind: "All" },
};

/** Read the persisted prefs from `localStorage`. Returns the default
 * blob on any failure path: key absent, JSON.parse throws, shape
 * mismatch, unknown vocab from a future schema, `localStorage` itself
 * unavailable. */
export function loadPartnerListPrefs(
  storage: Pick<Storage, "getItem"> | null = localStorageOrNull(),
): PartnerListPrefs {
  if (storage === null) return cloneDefault();
  let raw: string | null;
  try {
    raw = storage.getItem(PARTNER_LIST_PREFS_KEY);
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

/** Write the prefs blob to `localStorage`. Fire-and-forget on
 * setItem failures. */
export function savePartnerListPrefs(
  prefs: PartnerListPrefs,
  storage: Pick<Storage, "setItem"> | null = localStorageOrNull(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(PARTNER_LIST_PREFS_KEY, JSON.stringify(prefs));
  } catch (e) {
    // eslint-disable-next-line no-console
    console.warn("aberp: failed to persist partner list prefs", e);
  }
}

function cloneDefault(): PartnerListPrefs {
  return {
    sort: { ...DEFAULT_PARTNER_LIST_PREFS.sort },
    filter: { ...DEFAULT_PARTNER_LIST_PREFS.filter },
  };
}

function validatePrefs(parsed: unknown): PartnerListPrefs {
  if (parsed === null || typeof parsed !== "object") return cloneDefault();
  const obj = parsed as Record<string, unknown>;
  return {
    sort: validateSort(obj.sort),
    filter: validateFilter(obj.filter),
  };
}

function validateSort(raw: unknown): PartnerListPrefs["sort"] {
  if (raw === null || typeof raw !== "object") {
    return { ...DEFAULT_PARTNER_LIST_PREFS.sort };
  }
  const obj = raw as Record<string, unknown>;
  const dir = LEGAL_SORT_DIRS.includes(obj.dir as SortDir)
    ? (obj.dir as SortDir)
    : "asc";
  if (obj.key === null) return { key: null, dir };
  if (
    typeof obj.key === "string" &&
    LEGAL_PARTNER_SORT_KEYS.includes(obj.key as PartnerSortKey)
  ) {
    return { key: obj.key as PartnerSortKey, dir };
  }
  return { ...DEFAULT_PARTNER_LIST_PREFS.sort };
}

function validateFilter(raw: unknown): PartnerFilterSpec {
  if (raw === null || typeof raw !== "object") {
    return { ...DEFAULT_PARTNER_LIST_PREFS.filter };
  }
  const obj = raw as Record<string, unknown>;
  const needle = typeof obj.needle === "string" ? obj.needle : "";
  const kind = validateKindFacet(obj.kind);
  return { needle, kind };
}

function validateKindFacet(raw: unknown): PartnerKindFacet {
  if (typeof raw === "string" && LEGAL_PARTNER_KIND_FACETS.includes(raw as PartnerKindFacet)) {
    return raw as PartnerKindFacet;
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
