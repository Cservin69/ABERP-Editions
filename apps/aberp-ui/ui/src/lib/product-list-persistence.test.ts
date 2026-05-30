// PR-181 / session-181 — vitest pins for the product-list filter
// persistence helpers. Mirror of `partner-list-persistence.test.ts`
// (separate file because the keys + shapes diverge once sort + facets
// land in a future PR).

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
  it("save → load returns matching value", () => {
    const storage = makeStorage();
    const prefs: ProductListPrefs = { filter: { needle: "konzultáció" } };
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

  it("ignores unknown sibling fields (future-PR additive extension)", () => {
    // Forward-compat: a later PR may persist `sort: {...}` + a
    // currency/unit facet. A legacy reader still honours the needle.
    const storage = makeStorage({
      [PRODUCT_LIST_PREFS_KEY]: JSON.stringify({
        filter: { needle: "Liter@15C", currency: "HUF", unit: "PIECE" },
        sort: { key: "unit_price", dir: "asc" },
      }),
    });
    expect(loadProductListPrefs(storage).filter.needle).toBe("Liter@15C");
  });
});
