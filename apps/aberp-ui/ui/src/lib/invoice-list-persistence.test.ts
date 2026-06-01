// PR-175 / session-175 — vitest pins for the invoice-list sort +
// filter persistence helpers. Pure-helper coverage (CLAUDE.md
// rule 9): each pin assigns a load-bearing behaviour to a failure
// mode the brief named. The SPA's vitest setup has no jsdom layer,
// so the helpers are storage-injectable and the pins drive an
// in-memory stub mirroring the read/write surface of `Storage`.

import { describe, expect, it } from "vitest";

import {
  DEFAULT_INVOICE_LIST_PREFS,
  INVOICE_LIST_PREFS_KEY,
  loadInvoiceListPrefs,
  saveInvoiceListPrefs,
  type InvoiceListPrefs,
} from "./invoice-list-persistence";

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

describe("invoice-list-persistence — load defaults", () => {
  it("returns the default prefs when storage is empty", () => {
    const storage = makeStorage();
    expect(loadInvoiceListPrefs(storage)).toEqual(DEFAULT_INVOICE_LIST_PREFS);
  });

  it("returns defaults when localStorage itself is unavailable (null)", () => {
    expect(loadInvoiceListPrefs(null)).toEqual(DEFAULT_INVOICE_LIST_PREFS);
  });

  it("returns defaults on malformed JSON without throwing", () => {
    const storage = makeStorage({ [INVOICE_LIST_PREFS_KEY]: "{not json" });
    expect(loadInvoiceListPrefs(storage)).toEqual(DEFAULT_INVOICE_LIST_PREFS);
  });

  it("returns defaults when getItem itself throws", () => {
    const throwingStorage: Pick<Storage, "getItem"> = {
      getItem(_k: string): string | null {
        throw new Error("private browsing");
      },
    };
    expect(loadInvoiceListPrefs(throwingStorage as Storage)).toEqual(
      DEFAULT_INVOICE_LIST_PREFS,
    );
  });
});

describe("invoice-list-persistence — round trip", () => {
  it("save → load returns matching value", () => {
    const storage = makeStorage();
    const prefs: InvoiceListPrefs = {
      sort: { key: "total", dir: "desc" },
      filter: { needle: "ACME", state: "Finalized", currency: "HUF", row_kind: "All" },
    };
    saveInvoiceListPrefs(prefs, storage);
    expect(loadInvoiceListPrefs(storage)).toEqual(prefs);
  });

  it("preserves a null sort key (operator reset) across the round trip", () => {
    const storage = makeStorage();
    const prefs: InvoiceListPrefs = {
      sort: { key: null, dir: "asc" },
      filter: { needle: "", state: "All", currency: "EUR", row_kind: "All" },
    };
    saveInvoiceListPrefs(prefs, storage);
    expect(loadInvoiceListPrefs(storage)).toEqual(prefs);
  });

  it("save fire-and-forgets a throwing setItem (no rethrow)", () => {
    const throwingStorage: Pick<Storage, "setItem"> = {
      setItem(_k: string, _v: string): void {
        throw new Error("quota exceeded");
      },
    };
    expect(() =>
      saveInvoiceListPrefs(DEFAULT_INVOICE_LIST_PREFS, throwingStorage as Storage),
    ).not.toThrow();
  });

  // S192 — operator-visible recovery: after a quota-exceeded throw, a
  // subsequent load against the SAME storage MUST surface the
  // previously-persisted blob intact (NOT a half-written corrupted
  // fragment, NOT the default). The helper's atomic `setItem(key, json)`
  // posture is exactly the safety property this pin locks: a
  // throw-on-write either succeeds wholesale or leaves the prior value
  // alone — there is no partial write to recover from.
  it("save throw leaves prior persisted value intact for subsequent load", () => {
    // Prime the storage with a valid, fully-typed prior blob.
    const storage = makeStorage();
    const prior: InvoiceListPrefs = {
      sort: { key: "fiscal_year", dir: "desc" },
      filter: { needle: "ACME", state: "Finalized", currency: "HUF", row_kind: "All" },
    };
    saveInvoiceListPrefs(prior, storage);
    expect(loadInvoiceListPrefs(storage)).toEqual(prior);

    // Now attempt a save against a storage stub whose `setItem`
    // throws (the localStorage-full / private-browsing failure
    // mode). The shim DELEGATES the `getItem` half to the same
    // backing map, so the load-side of the contract still sees the
    // prior good value — modelling the real-browser DOMException
    // posture where a quota throw leaves the keyed slot untouched.
    const next: InvoiceListPrefs = {
      sort: { key: "total", dir: "asc" },
      filter: { needle: "X", state: "All", currency: "EUR", row_kind: "All" },
    };
    const throwingShim: Storage = {
      ...storage,
      setItem(_k: string, _v: string): void {
        throw new Error("DOMException: quota exceeded");
      },
    };
    expect(() => saveInvoiceListPrefs(next, throwingShim)).not.toThrow();

    // Subsequent load via the ORIGINAL non-throwing storage handle
    // returns the prior good value — the throw never half-overwrote
    // the JSON blob with corrupted bytes.
    expect(loadInvoiceListPrefs(storage)).toEqual(prior);
  });
});

describe("invoice-list-persistence — closed-vocab discipline", () => {
  it("discards an unknown sort key (renamed column) and falls back", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: "renamed_column_v2", dir: "asc" },
        filter: { needle: "", state: "All", currency: "All" },
      }),
    });
    const loaded = loadInvoiceListPrefs(storage);
    expect(loaded.sort).toEqual({ key: null, dir: "asc" });
  });

  it("discards an unknown sort direction and falls back to asc", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: "total", dir: "sideways" },
        filter: { needle: "", state: "All", currency: "All" },
      }),
    });
    const loaded = loadInvoiceListPrefs(storage);
    expect(loaded.sort).toEqual({ key: "total", dir: "asc" });
  });

  it("discards an unknown state facet (renamed lifecycle label) → All", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "x", state: "Archived", currency: "All" },
      }),
    });
    const loaded = loadInvoiceListPrefs(storage);
    expect(loaded.filter).toEqual({
      needle: "x",
      state: "All",
      currency: "All",
      row_kind: "All",
    });
  });

  it("discards an unknown currency (widening lag) → All", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "", state: "All", currency: "USD" },
      }),
    });
    expect(loadInvoiceListPrefs(storage).filter.currency).toBe("All");
  });

  it("accepts a known state facet verbatim", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "", state: "Submitted", currency: "All" },
      }),
    });
    expect(loadInvoiceListPrefs(storage).filter.state).toBe("Submitted");
  });

  it("falls back cleanly when only `sort` is persisted (legacy / partial blob)", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: "fiscal_year", dir: "desc" },
      }),
    });
    const loaded = loadInvoiceListPrefs(storage);
    expect(loaded.sort).toEqual({ key: "fiscal_year", dir: "desc" });
    expect(loaded.filter).toEqual({
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "All",
    });
  });

  it("coerces a non-string needle to empty (operator did not type one)", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: 42, state: "All", currency: "All" },
      }),
    });
    expect(loadInvoiceListPrefs(storage).filter.needle).toBe("");
  });

  // PR-213 / S215 — row_kind facet persistence pins. Three branches
  // per CLAUDE.md rule 9: known-vocab "Own" + "ExtNav" round-trip; an
  // unknown string discards to "All"; a legacy blob without row_kind
  // also defaults to "All" so an operator upgrading from pre-S215
  // persists transparently. The sort-key variant ("row_kind") also
  // round-trips through `LEGAL_SORT_KEYS`.
  it("round-trips row_kind facet = Own", () => {
    const storage = makeStorage();
    saveInvoiceListPrefs(
      {
        sort: { key: null, dir: "asc" },
        filter: { needle: "", state: "All", currency: "All", row_kind: "Own" },
      },
      storage,
    );
    expect(loadInvoiceListPrefs(storage).filter.row_kind).toBe("Own");
  });
  it("round-trips row_kind facet = ExtNav", () => {
    const storage = makeStorage();
    saveInvoiceListPrefs(
      {
        sort: { key: null, dir: "asc" },
        filter: { needle: "", state: "All", currency: "All", row_kind: "ExtNav" },
      },
      storage,
    );
    expect(loadInvoiceListPrefs(storage).filter.row_kind).toBe("ExtNav");
  });
  it("discards an unknown row_kind value back to All", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "", state: "All", currency: "All", row_kind: "Pending" },
      }),
    });
    expect(loadInvoiceListPrefs(storage).filter.row_kind).toBe("All");
  });
  it("defaults row_kind to All for a legacy persisted blob (no facet field)", () => {
    const storage = makeStorage({
      [INVOICE_LIST_PREFS_KEY]: JSON.stringify({
        sort: { key: null, dir: "asc" },
        filter: { needle: "", state: "All", currency: "All" },
      }),
    });
    expect(loadInvoiceListPrefs(storage).filter.row_kind).toBe("All");
  });
  it("round-trips sort.key = row_kind", () => {
    const storage = makeStorage();
    saveInvoiceListPrefs(
      {
        sort: { key: "row_kind", dir: "desc" },
        filter: { needle: "", state: "All", currency: "All", row_kind: "All" },
      },
      storage,
    );
    const loaded = loadInvoiceListPrefs(storage);
    expect(loaded.sort).toEqual({ key: "row_kind", dir: "desc" });
  });
});
