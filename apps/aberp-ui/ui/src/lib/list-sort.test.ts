// PR-194 / session-194 — vitest pins for the shared list-sort
// helpers. Each pin assigns a load-bearing behaviour to a failure
// mode the brief named.

import { describe, expect, it } from "vitest";

import { applySortDir, compareNullishLast, localeCompareHu } from "./list-sort";

describe("localeCompareHu", () => {
  it("returns 0 for identical strings", () => {
    expect(localeCompareHu("Árpád", "Árpád")).toBe(0);
  });

  it("orders Á between A and B per Hungarian collation", () => {
    // A byte-wise lex sort would cluster every accented character
    // at the bottom of the ASCII range (Á > Z); the locale-aware
    // compare places Á between A and B. Pinning the relative order
    // (not the exact tri-state magnitude — Intl.Collator may return
    // any negative integer, not specifically -1) keeps the pin
    // robust across browser engines.
    expect(localeCompareHu("Anna", "Árpád") < 0).toBe(true);
    expect(localeCompareHu("Árpád", "Béla") < 0).toBe(true);
    expect(localeCompareHu("Béla", "Árpád") > 0).toBe(true);
  });
});

describe("compareNullishLast", () => {
  it("returns null when both sides are non-null (delegate to typed cmp)", () => {
    expect(compareNullishLast("a", "b")).toBe(null);
    expect(compareNullishLast(1, 2)).toBe(null);
  });

  it("returns 0 when both sides are null (delegate to tiebreaker)", () => {
    expect(compareNullishLast(null, null)).toBe(0);
    expect(compareNullishLast(undefined, undefined)).toBe(0);
    expect(compareNullishLast(null, undefined)).toBe(0);
  });

  it("sorts a-null AFTER b-non-null regardless of direction", () => {
    // The sentinel direction is dir-invariant — the outer caller does
    // NOT apply the dir flip. A regression that returned -1 here
    // would cluster nulls at the TOP when ascending; pinning the
    // exact positive return guards against that flip.
    expect(compareNullishLast(null, "x")).toBe(1);
    expect(compareNullishLast(undefined, 42)).toBe(1);
  });

  it("sorts b-null AFTER a-non-null regardless of direction", () => {
    expect(compareNullishLast("x", null)).toBe(-1);
    expect(compareNullishLast(42, undefined)).toBe(-1);
  });
});

describe("applySortDir", () => {
  it("passes the cmp through unchanged on asc", () => {
    expect(applySortDir(-1, "asc")).toBe(-1);
    expect(applySortDir(0, "asc")).toBe(0);
    expect(applySortDir(1, "asc")).toBe(1);
  });

  it("flips the cmp on desc", () => {
    expect(applySortDir(-1, "desc")).toBe(1);
    expect(applySortDir(0, "desc")).toBe(-0); // -0 === 0 in compare
    expect(applySortDir(1, "desc")).toBe(-1);
  });
});
