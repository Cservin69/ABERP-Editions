// PR-181 / session-181 — vitest pins for the partner-list persistence
// helpers. PR-194 / session-194 — extended to cover sort + Kind facet.

import { describe, expect, it } from "vitest";

import {
  DEFAULT_PARTNER_LIST_PREFS,
  PARTNER_LIST_PREFS_KEY,
  loadPartnerListPrefs,
  savePartnerListPrefs,
  type PartnerListPrefs,
} from "./partner-list-persistence";

function makeStorage(initial: Record<string, string> = {}): Storage & {
  store: Record<string, string>;
} {
  const store: Record<string, string> = { ...initial };
  return {
    store,
    getItem(key: string): string | null {
      return Object.prototype.hasOwnProperty.call(store, key) ? store[key] : null;
    },
    setItem(key: string, value: string): void {
      store[key] = value;
    },
    removeItem(key: string): void {
      delete store[key];
    },
    clear(): void {
      for (const k of Object.keys(store)) delete store[k];
    },
    key(_i: number): string | null {
      return null;
    },
    get length(): number {
      return Object.keys(store).length;
    },
  } as Storage & { store: Record<string, string> };
}

describe("partner-list-persistence — load defaults", () => {
  it("returns the default prefs when storage is empty", () => {
    expect(loadPartnerListPrefs(makeStorage())).toEqual(DEFAULT_PARTNER_LIST_PREFS);
  });

  it("returns defaults when localStorage itself is unavailable (null)", () => {
    expect(loadPartnerListPrefs(null)).toEqual(DEFAULT_PARTNER_LIST_PREFS);
  });

  it("returns defaults on malformed JSON without throwing", () => {
    const storage = makeStorage({ [PARTNER_LIST_PREFS_KEY]: "{not json" });
    expect(loadPartnerListPrefs(storage)).toEqual(DEFAULT_PARTNER_LIST_PREFS);
  });

  it("returns defaults when getItem itself throws", () => {
    const throwingStorage: Pick<Storage, "getItem"> = {
      getItem(_k: string): string | null {
        throw new Error("private browsing");
      },
    };
    expect(loadPartnerListPrefs(throwingStorage as Storage)).toEqual(
      DEFAULT_PARTNER_LIST_PREFS,
    );
  });
});

describe("partner-list-persistence — round trip", () => {
  it("save → load returns matching value (PR-194 sort + kind facet)", () => {
    const storage = makeStorage();
    const prefs: PartnerListPrefs = {
      sort: { key: "display_name", dir: "desc" },
      filter: { needle: "ACME Kft.", kind: "Customer" },
    };
    savePartnerListPrefs(prefs, storage);
    expect(loadPartnerListPrefs(storage)).toEqual(prefs);
  });

  it("preserves a null sort key (operator reset) across the round trip", () => {
    const storage = makeStorage();
    const prefs: PartnerListPrefs = {
      sort: { key: null, dir: "asc" },
      filter: { needle: "", kind: "All" },
    };
    savePartnerListPrefs(prefs, storage);
    expect(loadPartnerListPrefs(storage)).toEqual(prefs);
  });

  it("save fire-and-forgets a throwing setItem (no rethrow)", () => {
    const throwingStorage: Pick<Storage, "setItem"> = {
      setItem(_k: string, _v: string): void {
        throw new Error("quota exceeded");
      },
    };
    expect(() =>
      savePartnerListPrefs(DEFAULT_PARTNER_LIST_PREFS, throwingStorage as Storage),
    ).not.toThrow();
  });
});

describe("partner-list-persistence — closed-vocab discipline", () => {
  it("coerces a non-string needle to empty (operator did not type one)", () => {
    const storage = makeStorage({
      [PARTNER_LIST_PREFS_KEY]: JSON.stringify({ filter: { needle: 42 } }),
    });
    expect(loadPartnerListPrefs(storage).filter.needle).toBe("");
  });

  it("falls back cleanly on a non-object root (legacy / future schema)", () => {
    const storage = makeStorage({ [PARTNER_LIST_PREFS_KEY]: JSON.stringify("bare-string") });
    expect(loadPartnerListPrefs(storage)).toEqual(DEFAULT_PARTNER_LIST_PREFS);
  });

  it("discards an unknown sort key (renamed column) and falls back", () => {
    const storage = makeStorage({
      [PARTNER_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: "renamed_column_v2", dir: "asc" },
        filter: { needle: "", kind: "All" },
      }),
    });
    const loaded = loadPartnerListPrefs(storage);
    expect(loaded.sort).toEqual({ key: null, dir: "asc" });
  });

  it("discards an unknown sort direction and falls back to asc", () => {
    const storage = makeStorage({
      [PARTNER_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: "kind", dir: "sideways" },
        filter: { needle: "", kind: "All" },
      }),
    });
    const loaded = loadPartnerListPrefs(storage);
    expect(loaded.sort).toEqual({ key: "kind", dir: "asc" });
  });

  it("discards an unknown kind facet (renamed vocab) → All", () => {
    const storage = makeStorage({
      [PARTNER_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "x", kind: "Vendor" },
      }),
    });
    expect(loadPartnerListPrefs(storage).filter.kind).toBe("All");
  });

  it("accepts every known kind facet verbatim", () => {
    for (const kind of ["All", "Customer", "Supplier", "Both"] as const) {
      const storage = makeStorage({
        [PARTNER_LIST_PREFS_KEY]: JSON.stringify({
          sort: { key: null, dir: "asc" },
          filter: { needle: "", kind },
        }),
      });
      expect(loadPartnerListPrefs(storage).filter.kind).toBe(kind);
    }
  });

  it("backward-compat: a legacy needle-only blob loads cleanly", () => {
    // PR-181 wrote `{ filter: { needle } }` with no sort + no kind
    // sibling. PR-194 readers must honour the needle and fall back to
    // defaults for the missing fields — never crash, never drop the
    // needle.
    const storage = makeStorage({
      [PARTNER_LIST_PREFS_KEY]: JSON.stringify({
        filter: { needle: "BÉLA" },
      }),
    });
    const loaded = loadPartnerListPrefs(storage);
    expect(loaded.filter.needle).toBe("BÉLA");
    expect(loaded.filter.kind).toBe("All");
    expect(loaded.sort).toEqual({ key: null, dir: "asc" });
  });

  it("ignores unknown sibling fields (future-PR additive extension)", () => {
    const storage = makeStorage({
      [PARTNER_LIST_PREFS_KEY]: JSON.stringify({
        filter: { needle: "BÉLA", kind: "Supplier" },
        sort: { key: "display_name", dir: "desc" },
        unknown_facet: "ignored",
      }),
    });
    const loaded = loadPartnerListPrefs(storage);
    expect(loaded.filter.needle).toBe("BÉLA");
    expect(loaded.filter.kind).toBe("Supplier");
    expect(loaded.sort).toEqual({ key: "display_name", dir: "desc" });
  });
});
