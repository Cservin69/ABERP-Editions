// PR-181 / session-181 — vitest pins for the partner-list filter
// persistence helpers. Pure-helper coverage (CLAUDE.md rule 9): each
// pin assigns a load-bearing behaviour to a failure mode the brief
// named. Storage-injectable so the SPA's no-jsdom vitest setup
// doesn't have to touch `window.localStorage`.

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
  it("save → load returns matching value", () => {
    const storage = makeStorage();
    const prefs: PartnerListPrefs = { filter: { needle: "ACME Kft." } };
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

  it("ignores unknown sibling fields (future-PR additive extension)", () => {
    // Forward-compat: a future PR may persist `sort: { key, dir }` next
    // to `filter`. A legacy reader that doesn't understand `sort` MUST
    // still honour the `filter.needle` it does understand.
    const storage = makeStorage({
      [PARTNER_LIST_PREFS_KEY]: JSON.stringify({
        filter: { needle: "BÉLA" },
        sort: { key: "display_name", dir: "desc" },
        unknown_facet: "ignored",
      }),
    });
    expect(loadPartnerListPrefs(storage).filter.needle).toBe("BÉLA");
  });
});
