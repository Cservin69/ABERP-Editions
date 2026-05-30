// S180 / PR-180 — pin the wizard's load-bearing gates.
//
// The brief explicitly named:
//   - "type RESTORE to enable submit" — operator-discipline ceremony.
//   - year bounds 2018..currentYear — backend mirror.
// Both are tested below without standing up a Svelte renderer.

import { describe, expect, it } from "vitest";

import {
  canSubmit,
  formatRestoreSummary,
  isRestoreConfirmed,
  MIN_RESTORE_YEAR,
  RESTORE_CONFIRMATION_TOKEN,
  validateYearInput,
} from "./restore-wizard";

describe("RESTORE confirmation gate", () => {
  it("accepts the exact uppercase token", () => {
    expect(isRestoreConfirmed("RESTORE")).toBe(true);
    expect(RESTORE_CONFIRMATION_TOKEN).toBe("RESTORE");
  });
  it("rejects lowercase / mixed-case", () => {
    expect(isRestoreConfirmed("restore")).toBe(false);
    expect(isRestoreConfirmed("Restore")).toBe(false);
    expect(isRestoreConfirmed("RESTOre")).toBe(false);
  });
  it("rejects whitespace-padded input — ceremony requires deliberate typing", () => {
    expect(isRestoreConfirmed(" RESTORE")).toBe(false);
    expect(isRestoreConfirmed("RESTORE ")).toBe(false);
    expect(isRestoreConfirmed(" RESTORE ")).toBe(false);
  });
  it("rejects partial / extra tokens", () => {
    expect(isRestoreConfirmed("")).toBe(false);
    expect(isRestoreConfirmed("REST")).toBe(false);
    expect(isRestoreConfirmed("RESTORES")).toBe(false);
    expect(isRestoreConfirmed("Y")).toBe(false);
  });
});

describe("year-bounds validator", () => {
  it("accepts current and past years inside the window", () => {
    expect(validateYearInput("2026", 2026)).toEqual({ kind: "ok", year: 2026 });
    expect(validateYearInput("2018", 2026)).toEqual({ kind: "ok", year: 2018 });
    expect(validateYearInput("2020", 2026)).toEqual({ kind: "ok", year: 2020 });
  });
  it("rejects below the NAV-introduction floor", () => {
    expect(validateYearInput("2017", 2026)).toEqual({
      kind: "below_floor",
      floor: MIN_RESTORE_YEAR,
    });
    expect(validateYearInput("1999", 2026)).toEqual({
      kind: "below_floor",
      floor: MIN_RESTORE_YEAR,
    });
  });
  it("rejects above the current calendar year", () => {
    expect(validateYearInput("2027", 2026)).toEqual({
      kind: "above_ceiling",
      ceiling: 2026,
    });
  });
  it("rejects non-integer inputs", () => {
    expect(validateYearInput("", 2026)).toEqual({ kind: "not_integer" });
    expect(validateYearInput("  ", 2026)).toEqual({ kind: "not_integer" });
    expect(validateYearInput("abc", 2026)).toEqual({ kind: "not_integer" });
    expect(validateYearInput("2026.5", 2026)).toEqual({ kind: "not_integer" });
    expect(validateYearInput("2e3", 2026)).toEqual({ kind: "not_integer" });
  });
  it("trims whitespace around the integer", () => {
    expect(validateYearInput("  2026  ", 2026)).toEqual({
      kind: "ok",
      year: 2026,
    });
  });
});

describe("canSubmit combines both gates", () => {
  it("returns true only when year valid AND token matches exactly", () => {
    expect(canSubmit("2026", "RESTORE", 2026)).toBe(true);
  });
  it("returns false when year invalid even if token matches", () => {
    expect(canSubmit("2017", "RESTORE", 2026)).toBe(false);
    expect(canSubmit("", "RESTORE", 2026)).toBe(false);
  });
  it("returns false when token wrong even if year valid", () => {
    expect(canSubmit("2026", "", 2026)).toBe(false);
    expect(canSubmit("2026", "restore", 2026)).toBe(false);
  });
});

describe("formatRestoreSummary", () => {
  it("names every count — silent omission would mask failures", () => {
    const s = formatRestoreSummary({
      year: 2026,
      restored: 3,
      skipped: 47,
      errored: 2,
      pages_walked: 14,
      elapsed_ms: 1234,
    });
    // All five counts MUST appear so the operator-visible string is
    // honest about what happened.
    expect(s).toContain("Year 2026");
    expect(s).toContain("3 restored");
    expect(s).toContain("47");
    expect(s).toContain("skipped");
    expect(s).toContain("2 errored");
    expect(s).toContain("14");
    expect(s).toContain("1234 ms");
  });
});
