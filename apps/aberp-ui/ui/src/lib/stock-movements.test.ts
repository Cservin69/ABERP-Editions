// S231 / PR-227 — pins on the inventory v1 SPA helpers.

import { describe, expect, test } from "vitest";

import {
  MANUAL_REASONS,
  MOVEMENT_REASON_LABELS,
  formatQty,
  reasonLabel,
} from "./stock-movements";

describe("MOVEMENT_REASON_LABELS", () => {
  test("covers every backend reason variant once", () => {
    const expected = [
      "receipt",
      "bom_consumption",
      "wo_completion",
      "adjustment",
      "dispatch",
      "scrap",
    ];
    const got = MOVEMENT_REASON_LABELS.map((r) => r.reason).sort();
    expect(got).toEqual(expected.sort());
  });

  test("MANUAL_REASONS matches ADR-0061 §6 — Receipt | Adjustment | Scrap", () => {
    // ADR-0061 §6: "Operator-supplied movements always have ref_kind =
    // Manual"; the manual form only offers Receipt / Adjustment / Scrap.
    // The other three (BomConsumption / WoCompletion / Dispatch) are
    // upstream-only per ADR-0062/0063/0064.
    expect([...MANUAL_REASONS].sort()).toEqual([
      "adjustment",
      "receipt",
      "scrap",
    ]);
  });
});

describe("reasonLabel", () => {
  test("returns bilingual labels for every reason", () => {
    expect(reasonLabel("receipt", "en")).toBe("Receipt");
    expect(reasonLabel("receipt", "hu")).toBe("Bevét");
    expect(reasonLabel("scrap", "hu")).toBe("Selejt");
    expect(reasonLabel("bom_consumption", "en")).toBe("BOM consumption");
  });
});

describe("formatQty", () => {
  test("strips DB trailing zeros from whole numbers", () => {
    expect(formatQty("10.000000")).toBe("10");
    expect(formatQty("0.000000")).toBe("0");
    expect(formatQty("-3.000000")).toBe("-3");
  });

  test("preserves real fractional digits", () => {
    expect(formatQty("-3.5")).toBe("-3.5");
    expect(formatQty("100.250000")).toBe("100.25");
    expect(formatQty("0.500000")).toBe("0.5");
  });

  test("integers without decimal point pass through unchanged", () => {
    expect(formatQty("10")).toBe("10");
    expect(formatQty("-5")).toBe("-5");
    expect(formatQty("0")).toBe("0");
  });
});
