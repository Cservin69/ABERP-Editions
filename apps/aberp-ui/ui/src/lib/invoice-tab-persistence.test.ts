// PR-179 / session-179 — vitest pins for the Outgoing/Incoming tab
// persistence helper. Storage-injectable; pins drive an in-memory stub
// mirroring `Storage`'s read/write surface (the SPA's vitest setup has
// no jsdom layer per the S175 convention).

import { describe, expect, it } from "vitest";

import {
  DEFAULT_INVOICE_TAB,
  INVOICE_TAB_KEY,
  loadInvoiceTab,
  saveInvoiceTab,
} from "./invoice-tab-persistence";

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

describe("invoice-tab-persistence — load", () => {
  it("returns Outgoing default on empty storage (first launch)", () => {
    expect(loadInvoiceTab(makeStorage())).toBe("outgoing");
  });

  it("returns Outgoing default when localStorage itself is unavailable", () => {
    expect(loadInvoiceTab(null)).toBe(DEFAULT_INVOICE_TAB);
  });

  it("returns the persisted value when valid", () => {
    const storage = makeStorage({ [INVOICE_TAB_KEY]: "incoming" });
    expect(loadInvoiceTab(storage)).toBe("incoming");
  });

  it("discards unknown vocab and falls back to default", () => {
    const storage = makeStorage({ [INVOICE_TAB_KEY]: "archive" });
    expect(loadInvoiceTab(storage)).toBe("outgoing");
  });

  it("discards empty string and falls back to default", () => {
    const storage = makeStorage({ [INVOICE_TAB_KEY]: "" });
    expect(loadInvoiceTab(storage)).toBe("outgoing");
  });

  it("returns defaults when getItem throws (private browsing path)", () => {
    const storage = {
      getItem: () => {
        throw new Error("blocked");
      },
    };
    expect(loadInvoiceTab(storage)).toBe("outgoing");
  });
});

describe("invoice-tab-persistence — save", () => {
  it("round-trips outgoing", () => {
    const storage = makeStorage();
    saveInvoiceTab("outgoing", storage);
    expect(loadInvoiceTab(storage)).toBe("outgoing");
  });

  it("round-trips incoming", () => {
    const storage = makeStorage();
    saveInvoiceTab("incoming", storage);
    expect(loadInvoiceTab(storage)).toBe("incoming");
  });

  it("round-trips quotes (S211)", () => {
    const storage = makeStorage();
    saveInvoiceTab("quotes", storage);
    expect(loadInvoiceTab(storage)).toBe("quotes");
  });

  it("does not throw when localStorage is unavailable", () => {
    expect(() => saveInvoiceTab("incoming", null)).not.toThrow();
  });

  it("does not throw when setItem throws (quota exceeded)", () => {
    const storage = {
      setItem: () => {
        throw new Error("quota");
      },
    };
    expect(() => saveInvoiceTab("incoming", storage)).not.toThrow();
  });
});
