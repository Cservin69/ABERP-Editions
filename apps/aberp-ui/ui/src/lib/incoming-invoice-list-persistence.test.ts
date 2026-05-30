// PR-179 / session-179 — vitest pins for the IncomingInvoiceList
// sort + filter persistence helpers. Pattern mirrors the AR-side
// `invoice-list-persistence.test.ts` (S175) — storage-injectable,
// in-memory stub mirroring the read/write surface of `Storage`.

import { describe, expect, it } from "vitest";

import {
  DEFAULT_INCOMING_LIST_PREFS,
  EMPTY_INCOMING_FILTER,
  INCOMING_LIST_PREFS_KEY,
  loadIncomingListPrefs,
  saveIncomingListPrefs,
  type IncomingListPrefs,
} from "./incoming-invoice-list-persistence";

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

describe("incoming-invoice-list-persistence — defaults", () => {
  it("returns the empty default when storage is empty", () => {
    expect(loadIncomingListPrefs(makeStorage())).toEqual(
      DEFAULT_INCOMING_LIST_PREFS,
    );
  });

  it("returns the default when storage is null (private browsing)", () => {
    expect(loadIncomingListPrefs(null)).toEqual(DEFAULT_INCOMING_LIST_PREFS);
  });

  it("returns the default on malformed JSON without throwing", () => {
    const storage = makeStorage({ [INCOMING_LIST_PREFS_KEY]: "{not json" });
    expect(loadIncomingListPrefs(storage)).toEqual(DEFAULT_INCOMING_LIST_PREFS);
  });

  it("returns the default when getItem throws", () => {
    const storage = {
      getItem: () => {
        throw new Error("blocked");
      },
    };
    expect(loadIncomingListPrefs(storage)).toEqual(DEFAULT_INCOMING_LIST_PREFS);
  });
});

describe("incoming-invoice-list-persistence — round-trip", () => {
  const sample: IncomingListPrefs = {
    sort: { key: "issue_date", dir: "desc" },
    filter: { needle: "Acme", status: "Outstanding", currency: "EUR" },
  };

  it("round-trips a fully-populated prefs blob", () => {
    const storage = makeStorage();
    saveIncomingListPrefs(sample, storage);
    expect(loadIncomingListPrefs(storage)).toEqual(sample);
  });

  it("does not throw when storage is unavailable", () => {
    expect(() => saveIncomingListPrefs(sample, null)).not.toThrow();
  });

  it("does not throw when setItem throws (quota exceeded)", () => {
    const storage = {
      setItem: () => {
        throw new Error("quota");
      },
    };
    expect(() => saveIncomingListPrefs(sample, storage)).not.toThrow();
  });
});

describe("incoming-invoice-list-persistence — closed-vocab discards", () => {
  it("discards an unknown sort key + falls back to default sort", () => {
    const storage = makeStorage({
      [INCOMING_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: "supplier_email_subdomain", dir: "asc" },
        filter: EMPTY_INCOMING_FILTER,
      }),
    });
    const loaded = loadIncomingListPrefs(storage);
    expect(loaded.sort.key).toBeNull();
  });

  it("discards an unknown status facet + falls back to All", () => {
    const storage = makeStorage({
      [INCOMING_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "", status: "Disputed", currency: "All" },
      }),
    });
    expect(loadIncomingListPrefs(storage).filter.status).toBe("All");
  });

  it("discards an unknown currency facet + falls back to All", () => {
    const storage = makeStorage({
      [INCOMING_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "", status: "All", currency: "GBP" },
      }),
    });
    expect(loadIncomingListPrefs(storage).filter.currency).toBe("All");
  });
});
