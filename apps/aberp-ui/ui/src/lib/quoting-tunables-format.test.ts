// S267 / PR-256 — vitest pin for the shared display helpers.
// Closed-vocab labels are stable; if a Rust-side enum variant lands
// without a corresponding TS arm, the runtime falls through to the
// verbatim string — these tests catch the "lost a label" regression.

import { describe, expect, it } from "vitest";

import {
  featureTypeLabel,
  fmtPct,
  generalClassLabel,
  sizeBucketLabel,
  toleranceRangeLabel,
  toleranceSpecLabel,
} from "./quoting-tunables-format";
import { FEATURE_TYPES, SIZE_BUCKETS, TOLERANCE_RANGES } from "./api";

describe("featureTypeLabel", () => {
  it("returns a non-empty label for every known feature type", () => {
    for (const t of FEATURE_TYPES) {
      const label = featureTypeLabel(t);
      expect(label.trim().length).toBeGreaterThan(0);
      expect(label).not.toBe(t); // SHOULD differ — falls through to wire form on unknown
    }
  });

  it("returns the verbatim string for an unknown variant", () => {
    expect(featureTypeLabel("nope")).toBe("nope");
  });
});

describe("sizeBucketLabel", () => {
  it("returns a label containing the bucket name for every known bucket", () => {
    for (const b of SIZE_BUCKETS) {
      expect(sizeBucketLabel(b)).toContain(b);
    }
  });
});

describe("toleranceRangeLabel", () => {
  it("returns a bilingual or compact label for every known range", () => {
    for (const t of TOLERANCE_RANGES) {
      const label = toleranceRangeLabel(t);
      expect(label.trim().length).toBeGreaterThan(0);
      expect(label).not.toBe(t); // closed-vocab gets a friendly label
    }
  });
});

describe("fmtPct", () => {
  it("formats negative, zero, and positive fractions with a sign mark", () => {
    expect(fmtPct(-0.05)).toBe("−5.0%");
    expect(fmtPct(0)).toBe("0.0%");
    expect(fmtPct(0.10)).toBe("+10.0%");
    expect(fmtPct(0.001)).toBe("+0.1%");
  });

  it("returns an em-dash on NaN/Inf", () => {
    expect(fmtPct(Number.NaN)).toBe("—");
    expect(fmtPct(Number.POSITIVE_INFINITY)).toBe("—");
  });
});

describe("generalClassLabel (T5)", () => {
  it("labels every ISO 2768 class, verbatim fallback on unknown", () => {
    for (const c of [
      "iso2768_fine",
      "iso2768_medium",
      "iso2768_coarse",
      "iso2768_very_coarse",
    ]) {
      const label = generalClassLabel(c);
      expect(label.trim().length).toBeGreaterThan(0);
      expect(label).not.toBe(c);
    }
    expect(generalClassLabel("nope")).toBe("nope");
  });
});

describe("toleranceSpecLabel (T5)", () => {
  it("renders each drawing dialect compactly", () => {
    expect(toleranceSpecLabel({ kind: "unspecified" })).toContain("Unspecified");
    expect(
      toleranceSpecLabel({ kind: "general_class", class: "iso2768_fine" }),
    ).toContain("ISO 2768-f");
    expect(toleranceSpecLabel({ kind: "it_grade", grade: 7 })).toBe("IT7");
    expect(toleranceSpecLabel({ kind: "plus_minus", value_mm: 0.01 })).toBe(
      "±0.01 mm",
    );
    expect(toleranceSpecLabel({ kind: "per_drawing" })).toContain("Per drawing");
  });
});
