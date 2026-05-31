// PR-181 / session-181 — vitest pins for the product-list persistence
// helpers. PR-194 / session-194 — extended to cover sort + Unit +
// Currency facets.

import { describe, expect, it } from "vitest";

import {
  DEFAULT_PRODUCT_LIST_PREFS,
  PRODUCT_LIST_PREFS_KEY,
  loadProductListPrefs,
  saveProductListPrefs,
  type ProductListPrefs,
} from "./product-list-persistence";

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

describe("product-list-persistence — load defaults", () => {
  it("returns the default prefs when storage is empty", () => {
    expect(loadProductListPrefs(makeStorage())).toEqual(DEFAULT_PRODUCT_LIST_PREFS);
  });

  it("returns defaults when localStorage itself is unavailable (null)", () => {
    expect(loadProductListPrefs(null)).toEqual(DEFAULT_PRODUCT_LIST_PREFS);
  });

  it("returns defaults on malformed JSON without throwing", () => {
    const storage = makeStorage({ [PRODUCT_LIST_PREFS_KEY]: "}{ bad" });
    expect(loadProductListPrefs(storage)).toEqual(DEFAULT_PRODUCT_LIST_PREFS);
  });

  it("returns defaults when getItem itself throws", () => {
    const throwingStorage: Pick<Storage, "getItem"> = {
      getItem(_k: string): string | null {
        throw new Error("private browsing");
      },
    };
    expect(loadProductListPrefs(throwingStorage as Storage)).toEqual(
      DEFAULT_PRODUCT_LIST_PREFS,
    );
  });
});

describe("product-list-persistence — round trip", () => {
  it("save → load returns matching value (PR-194 sort + unit + currency)", () => {
    const storage = makeStorage();
    const prefs: ProductListPrefs = {
      sort: { key: "price", dir: "desc" },
      filter: { needle: "konzultáció", unit: "Nav:HOUR", currency: "HUF" },
    };
    saveProductListPrefs(prefs, storage);
    expect(loadProductListPrefs(storage)).toEqual(prefs);
  });

  it("preserves a null sort key (operator reset) across the round trip", () => {
    const storage = makeStorage();
    const prefs: ProductListPrefs = {
      sort: { key: null, dir: "asc" },
      filter: { needle: "", unit: "All", currency: "EUR" },
    };
    saveProductListPrefs(prefs, storage);
    expect(loadProductListPrefs(storage)).toEqual(prefs);
  });

  it("save fire-and-forgets a throwing setItem (no rethrow)", () => {
    const throwingStorage: Pick<Storage, "setItem"> = {
      setItem(_k: string, _v: string): void {
        throw new Error("quota exceeded");
      },
    };
    expect(() =>
      saveProductListPrefs(DEFAULT_PRODUCT_LIST_PREFS, throwingStorage as Storage),
    ).not.toThrow();
  });
});

describe("product-list-persistence — closed-vocab discipline", () => {
  it("coerces a non-string needle to empty (operator did not type one)", () => {
    const storage = makeStorage({
      [PRODUCT_LIST_PREFS_KEY]: JSON.stringify({ filter: { needle: ["array"] } }),
    });
    expect(loadProductListPrefs(storage).filter.needle).toBe("");
  });

  it("falls back cleanly on a non-object root (legacy / future schema)", () => {
    const storage = makeStorage({ [PRODUCT_LIST_PREFS_KEY]: JSON.stringify(null) });
    expect(loadProductListPrefs(storage)).toEqual(DEFAULT_PRODUCT_LIST_PREFS);
  });

  it("discards an unknown sort key and falls back", () => {
    const storage = makeStorage({
      [PRODUCT_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: "renamed_v2", dir: "asc" },
        filter: { needle: "", unit: "All", currency: "All" },
      }),
    });
    expect(loadProductListPrefs(storage).sort).toEqual({ key: null, dir: "asc" });
  });

  it("discards an unknown sort direction and falls back to asc", () => {
    const storage = makeStorage({
      [PRODUCT_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: "price", dir: "sideways" },
        filter: { needle: "", unit: "All", currency: "All" },
      }),
    });
    expect(loadProductListPrefs(storage).sort).toEqual({ key: "price", dir: "asc" });
  });

  it("discards an unknown currency (widening lag) → All", () => {
    const storage = makeStorage({
      [PRODUCT_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "", unit: "All", currency: "USD" },
      }),
    });
    expect(loadProductListPrefs(storage).filter.currency).toBe("All");
  });

  it("accepts an arbitrary Unit facet string verbatim (open-ended vocab)", () => {
    // The Unit facet vocab is NOT compile-time fixed (the operator
    // can coin Own:<label> values). The persistence validator accepts
    // any non-empty string; the component-level renderer resets to
    // "All" if the persisted value matches no current row.
    const storage = makeStorage({
      [PRODUCT_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "", unit: "Own:liter@15C", currency: "All" },
      }),
    });
    expect(loadProductListPrefs(storage).filter.unit).toBe("Own:liter@15C");
  });

  it("backward-compat: a legacy needle-only blob loads cleanly", () => {
    // PR-181 wrote `{ filter: { needle } }` with no sort + no
    // unit/currency siblings. PR-194 readers honour the needle and
    // fall back to defaults for the missing fields.
    const storage = makeStorage({
      [PRODUCT_LIST_PREFS_KEY]: JSON.stringify({
        filter: { needle: "Liter@15C" },
      }),
    });
    const loaded = loadProductListPrefs(storage);
    expect(loaded.filter.needle).toBe("Liter@15C");
    expect(loaded.filter.unit).toBe("All");
    expect(loaded.filter.currency).toBe("All");
    expect(loaded.sort).toEqual({ key: null, dir: "asc" });
  });

  it("ignores unknown sibling fields (future-PR additive extension)", () => {
    const storage = makeStorage({
      [PRODUCT_LIST_PREFS_KEY]: JSON.stringify({
        filter: { needle: "Liter@15C", currency: "HUF", unit: "Nav:LITER" },
        sort: { key: "price", dir: "asc" },
        unknown_facet: "ignored",
      }),
    });
    const loaded = loadProductListPrefs(storage);
    expect(loaded.filter).toEqual({
      needle: "Liter@15C",
      unit: "Nav:LITER",
      currency: "HUF",
    });
    expect(loaded.sort).toEqual({ key: "price", dir: "asc" });
  });
});
