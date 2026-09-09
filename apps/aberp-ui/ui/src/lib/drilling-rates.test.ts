// ADR-0112 Part C (D-19 slice C3) — pin the pure drilling-rate form helpers.

import { describe, expect, it } from "vitest";
import type { DrillingRate } from "./api";
import {
  composeDrillingRateInputs,
  drillingRateStatusLabel,
  emptyDrillingRateForm,
  formFromDrillingRate,
  isInertDrillingRate,
} from "./drilling-rates";

function rate(overrides: Partial<DrillingRate> = {}): DrillingRate {
  return {
    id: "qdr_01ABC",
    material_group: "Ti-6Al-4V",
    feed_mm_per_min_per_mm_dia: 0,
    peck_depth_dia_multiple: 2,
    peck_retract_sec: 1.2,
    rapid_per_hole_sec: 2.5,
    tool_change_sec: 30,
    flat_bottom_factor: 1.5,
    unknown_end_condition_factor: 1.7,
    notes: "SEED — INERT",
    updated_at: "2026-09-09T00:00:00Z",
    updated_by_actor: "boot",
    ...overrides,
  };
}

describe("emptyDrillingRateForm", () => {
  it("defaults to an inert row (feed 0) with a blank material group", () => {
    const form = emptyDrillingRateForm();
    expect(form.materialGroup).toBe("");
    expect(form.feedMmPerMinPerMmDia).toBe("0");
  });
});

describe("form ↔ wire round-trip", () => {
  it("folds a fetched rate into form state and back to the same numbers", () => {
    const r = rate({ feed_mm_per_min_per_mm_dia: 120, material_group: "6061-T6" });
    const form = formFromDrillingRate(r);
    expect(form.materialGroup).toBe("6061-T6");
    expect(form.feedMmPerMinPerMmDia).toBe("120");

    const body = composeDrillingRateInputs(form);
    expect(body.material_group).toBe("6061-T6");
    expect(body.feed_mm_per_min_per_mm_dia).toBe(120);
    expect(body.unknown_end_condition_factor).toBe(1.7);
  });

  it("trims the material group and maps a blank notes to null", () => {
    const form = { ...emptyDrillingRateForm(), materialGroup: "  304  ", notes: "   " };
    const body = composeDrillingRateInputs(form);
    expect(body.material_group).toBe("304");
    expect(body.notes).toBeNull();
  });

  it("passes an unparseable numeric through as NaN for the backend to reject", () => {
    const form = { ...emptyDrillingRateForm(), feedMmPerMinPerMmDia: "abc" };
    expect(Number.isNaN(composeDrillingRateInputs(form).feed_mm_per_min_per_mm_dia)).toBe(true);
  });
});

describe("isInertDrillingRate", () => {
  it("treats feed 0 and any non-positive feed as inert", () => {
    expect(isInertDrillingRate(rate({ feed_mm_per_min_per_mm_dia: 0 }))).toBe(true);
    expect(isInertDrillingRate(rate({ feed_mm_per_min_per_mm_dia: -1 }))).toBe(true);
    expect(isInertDrillingRate(rate({ feed_mm_per_min_per_mm_dia: 0.001 }))).toBe(false);
  });
});

describe("drillingRateStatusLabel", () => {
  it("says inert loudly for a feed-0 row and shows the feed when active", () => {
    expect(drillingRateStatusLabel(rate({ feed_mm_per_min_per_mm_dia: 0 }))).toContain("inert");
    expect(drillingRateStatusLabel(rate({ feed_mm_per_min_per_mm_dia: 120 }))).toContain("active");
  });
});
