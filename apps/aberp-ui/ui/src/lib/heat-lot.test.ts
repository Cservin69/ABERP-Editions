// S432 — vitest pins for the heat-lot client-side validators.

import { describe, it, expect } from "vitest";

import { validateHeatLot, validateMtrUrl } from "./heat-lot";

describe("validateHeatLot", () => {
  it("accepts a non-empty alphanumeric+dash lot", () => {
    expect(validateHeatLot("HL-2026-007")).toBeNull();
    expect(validateHeatLot("ABC123")).toBeNull();
    // trims surrounding whitespace before checking
    expect(validateHeatLot("  HL-1  ")).toBeNull();
  });

  it("rejects an empty / whitespace-only lot", () => {
    expect(validateHeatLot("")).not.toBeNull();
    expect(validateHeatLot("   ")).not.toBeNull();
  });

  it("rejects a bad character", () => {
    expect(validateHeatLot("HL 1")).not.toBeNull();
    expect(validateHeatLot("HL_1")).not.toBeNull();
    expect(validateHeatLot("HL/1")).not.toBeNull();
  });

  it("rejects a lot longer than 32 chars", () => {
    expect(validateHeatLot("A".repeat(32))).toBeNull();
    expect(validateHeatLot("A".repeat(33))).not.toBeNull();
  });
});

describe("validateMtrUrl", () => {
  it("accepts an empty value (no MTR yet)", () => {
    expect(validateMtrUrl("")).toBeNull();
    expect(validateMtrUrl("   ")).toBeNull();
  });

  it("accepts a file:// url", () => {
    expect(validateMtrUrl("file:///srv/mtr/heat-007.pdf")).toBeNull();
  });

  it("rejects a non-file:// url", () => {
    expect(validateMtrUrl("https://example.com/mtr.pdf")).not.toBeNull();
    expect(validateMtrUrl("/srv/mtr/heat-007.pdf")).not.toBeNull();
  });
});
