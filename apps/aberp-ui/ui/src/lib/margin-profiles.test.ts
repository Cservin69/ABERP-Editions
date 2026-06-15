// S428 — pins for the margin-profiles pure helpers (percent ↔ fraction
// round-trip, form composition, filter, validation-body parser).

import { describe, expect, it } from "vitest";

import type { MarginProfile } from "./api";
import {
  composeMarginProfileInputs,
  emptyMarginProfileForm,
  filterProfiles,
  formatPercent,
  formFromProfile,
  fractionToPercentString,
  parseMarginProfileValidationError,
  percentStringToFraction,
} from "./margin-profiles";

function profile(overrides: Partial<MarginProfile>): MarginProfile {
  return {
    id: "mp_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    name: "Defense",
    customer_type: "defense",
    gross_margin_pct: 0.4,
    min_margin_pct: 0.1,
    notes: null,
    enabled: true,
    created_at: "2026-06-15T00:00:00Z",
    updated_at: "2026-06-15T00:00:00Z",
    archived_at: null,
    ...overrides,
  };
}

describe("percent ↔ fraction", () => {
  it("round-trips a whole percent", () => {
    expect(percentStringToFraction("35")).toBeCloseTo(0.35, 10);
    expect(fractionToPercentString(0.35)).toBe("35");
  });

  it("formats a fraction as a percent label", () => {
    expect(formatPercent(0.1)).toBe("10%");
    expect(formatPercent(0.125)).toBe("12.5%");
  });
});

describe("form composition", () => {
  it("empty form carries the day-1 defaults as percent strings", () => {
    const f = emptyMarginProfileForm();
    expect(f.grossMarginPct).toBe("35");
    expect(f.minMarginPct).toBe("10");
    expect(f.enabled).toBe(true);
  });

  it("composes percent strings back to wire fractions", () => {
    const f = emptyMarginProfileForm();
    f.name = "  Defense  ";
    f.customerType = "defense";
    f.grossMarginPct = "40";
    f.minMarginPct = "10";
    f.notes = "  high-floor segment  ";
    const body = composeMarginProfileInputs(f);
    expect(body.name).toBe("Defense");
    expect(body.gross_margin_pct).toBeCloseTo(0.4, 10);
    expect(body.min_margin_pct).toBeCloseTo(0.1, 10);
    expect(body.notes).toBe("high-floor segment");
  });

  it("collapses empty notes to null", () => {
    const f = emptyMarginProfileForm();
    f.notes = "   ";
    expect(composeMarginProfileInputs(f).notes).toBeNull();
  });

  it("round-trips a fetched profile through formFromProfile", () => {
    const f = formFromProfile(profile({ gross_margin_pct: 0.4, min_margin_pct: 0.1 }));
    expect(f.grossMarginPct).toBe("40");
    expect(f.minMarginPct).toBe("10");
    expect(f.customerType).toBe("defense");
  });
});

describe("filterProfiles", () => {
  const rows = [
    profile({ id: "mp_a", name: "Defense", customer_type: "defense" }),
    profile({ id: "mp_b", name: "Consumer goods", customer_type: "consumer" }),
  ];

  it("returns all rows for an empty needle", () => {
    expect(filterProfiles(rows, "  ")).toHaveLength(2);
  });

  it("matches on name", () => {
    expect(filterProfiles(rows, "consumer goods").map((r) => r.id)).toEqual([
      "mp_b",
    ]);
  });

  it("matches on customer type", () => {
    expect(filterProfiles(rows, "defense").map((r) => r.id)).toEqual(["mp_a"]);
  });
});

describe("parseMarginProfileValidationError", () => {
  it("peels a wrapped validation body", () => {
    const raw =
      'error returned: {"error":"validation_failed","fields":[{"field":"gross_margin_pct","message":"bad"}]}';
    const parsed = parseMarginProfileValidationError(raw);
    expect(parsed?.fields[0]?.field).toBe("gross_margin_pct");
  });

  it("returns null for a non-validation shape", () => {
    expect(parseMarginProfileValidationError("plain error")).toBeNull();
    expect(
      parseMarginProfileValidationError('{"error":"below_margin_floor"}'),
    ).toBeNull();
  });
});
