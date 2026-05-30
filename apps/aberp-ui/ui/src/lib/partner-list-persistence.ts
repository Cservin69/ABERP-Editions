// PR-181 / session-181 — persist the PartnersList's quick-filter
// needle to `localStorage` so an operator's typed filter survives a
// page reload / app restart.
//
// Scope note (CLAUDE.md rule 3 — surgical changes):
// PartnersList today has ONLY a needle search input — no sortable
// column headers, no kind facet chip. Per the session-181 brief, this
// PR does NOT introduce sortability or a kind facet (that would be an
// out-of-scope UI expansion). The persisted shape carries `filter`
// alone; when sort columns or a kind facet land in a later PR, this
// helper extends additively (add a `sort` sibling + a `kind` field on
// `filter`; the validator drops unknown fields on legacy blobs).
//
// Independent of the AR / AP list prefs (separate key) so the
// operator can keep orthogonal views across surfaces. Storage-injectable
// for vitest pins (the SPA's vitest setup has no jsdom layer; the pin
// drives an in-memory stub mirroring the read/write surface).
//
// Closed-vocab discipline mirrors `invoice-list-persistence.ts` and
// `incoming-invoice-list-persistence.ts`: a non-string needle in the
// persisted blob is coerced to empty (not "[object Object]"), and any
// shape mismatch falls back to the default. CLAUDE.md rule 7 — discard
// stale vocab, don't average it.
//
// Pinned by `partner-list-persistence.test.ts`.

/** Storage key. Namespaced under `aberp:` per the convention
 * established by `aberp:just-issued-invoice-id` (PR-87) and continued
 * by `aberp:invoice-list:prefs` (PR-175) + `aberp:incoming-invoice-list:prefs`
 * (PR-179). */
export const PARTNER_LIST_PREFS_KEY = "aberp:partner-list:prefs";

export interface PartnerListPrefs {
  filter: { needle: string };
}

export const DEFAULT_PARTNER_LIST_PREFS: PartnerListPrefs = {
  filter: { needle: "" },
};

/** Read the persisted prefs from `localStorage`. Returns the default
 * blob on any failure path: key absent, JSON.parse throws, shape
 * mismatch, `localStorage` itself unavailable. */
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

/** Write the prefs blob to `localStorage`. Fire-and-forget: a throw
 * from `setItem` (private browsing, quota exceeded) surfaces as a
 * `console.warn` so a regression that silently drops every save is
 * visible in the devtools console, without breaking the operator's
 * interaction. */
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
  return { filter: { ...DEFAULT_PARTNER_LIST_PREFS.filter } };
}

function validatePrefs(parsed: unknown): PartnerListPrefs {
  if (parsed === null || typeof parsed !== "object") return cloneDefault();
  const obj = parsed as Record<string, unknown>;
  return { filter: validateFilter(obj.filter) };
}

function validateFilter(raw: unknown): PartnerListPrefs["filter"] {
  if (raw === null || typeof raw !== "object") {
    return { ...DEFAULT_PARTNER_LIST_PREFS.filter };
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
