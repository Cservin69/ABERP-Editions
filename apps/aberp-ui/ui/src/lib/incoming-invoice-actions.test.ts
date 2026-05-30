// PR-179 / session-179 — vitest pins for the IncomingInvoice action
// helpers. Pinned behaviours:
//   - empty / whitespace reasons are rejected; non-empty are accepted.
//   - the sync-completed toast copy is correct across the 0 / 1 / N
//     paths (plurality matters for HU + EN).

import { describe, expect, it } from "vitest";

import {
  isIrrelevantReasonValid,
  syncCompletedToast,
} from "./incoming-invoice-actions";

describe("incoming-invoice-actions — isIrrelevantReasonValid", () => {
  it("rejects the empty string", () => {
    expect(isIrrelevantReasonValid("")).toBe(false);
  });

  it("rejects whitespace-only reasons (spaces, tabs, newlines)", () => {
    expect(isIrrelevantReasonValid("   ")).toBe(false);
    expect(isIrrelevantReasonValid("\t\n  ")).toBe(false);
  });

  it("accepts a one-character reason", () => {
    expect(isIrrelevantReasonValid("x")).toBe(true);
  });

  it("accepts a reason with surrounding whitespace if any visible char", () => {
    expect(isIrrelevantReasonValid("  duplicate  ")).toBe(true);
  });
});

describe("incoming-invoice-actions — syncCompletedToast", () => {
  it("returns 'no new' copy when ingested === 0", () => {
    const t = syncCompletedToast(0);
    expect(t.en.toLowerCase()).toContain("no new");
    expect(t.hu).toContain("nincs új");
  });

  it("singular sentence at exactly 1", () => {
    const t = syncCompletedToast(1);
    expect(t.en).toContain("1 new incoming invoice.");
    expect(t.hu).toContain("1 új bejövő számla");
  });

  it("plural sentence above 1, with the count interpolated", () => {
    const t = syncCompletedToast(7);
    expect(t.en).toContain("7 new incoming invoices.");
    expect(t.hu).toContain("7 új bejövő számla.");
  });
});
