// T5 / ADR-0097 Part 2 — vitest pin for the per-job tolerance editor helpers.
// Guards the draft↔wire round-trip, the inert detector (clear-to-default),
// and per-kind spec composition, so a drift from the route's `ToleranceBody`
// contract surfaces here.

import { describe, expect, it } from "vitest";

import {
  composeSpec,
  composeToleranceBody,
  emptyToleranceEditorState,
  parseToleranceSpecJson,
  toleranceEditorIsInert,
} from "./job-tolerance";

describe("composeSpec", () => {
  it("composes each drawing dialect into the engine wire shape", () => {
    expect(
      composeSpec({
        kind: "general_class",
        generalClass: "iso2768_fine",
        itGrade: "7",
        valueMm: "0.01",
      }),
    ).toEqual({ kind: "general_class", class: "iso2768_fine" });
    expect(
      composeSpec({
        kind: "it_grade",
        generalClass: "iso2768_medium",
        itGrade: "6",
        valueMm: "0.01",
      }),
    ).toEqual({ kind: "it_grade", grade: 6 });
    expect(
      composeSpec({
        kind: "plus_minus",
        generalClass: "iso2768_medium",
        itGrade: "7",
        valueMm: "0.02",
      }),
    ).toEqual({ kind: "plus_minus", value_mm: 0.02 });
    expect(
      composeSpec({
        kind: "per_drawing",
        generalClass: "iso2768_medium",
        itGrade: "7",
        valueMm: "0.01",
      }),
    ).toEqual({ kind: "per_drawing" });
  });
});

describe("parseToleranceSpecJson / composeToleranceBody round-trip", () => {
  it("round-trips a stored spec blob through the editor back to the wire body", () => {
    const stored = JSON.stringify({
      overall: { kind: "it_grade", grade: 7 },
      critical_features: [
        { feature_index: 2, spec: { kind: "plus_minus", value_mm: 0.01 } },
      ],
    });
    const body = composeToleranceBody(parseToleranceSpecJson(stored));
    expect(body).toEqual({
      overall: { kind: "it_grade", grade: 7 },
      critical_features: [
        { feature_index: 2, spec: { kind: "plus_minus", value_mm: 0.01 } },
      ],
    });
  });

  it("treats null / empty / {} as the inert empty state", () => {
    for (const j of [null, "", "  ", "{}"]) {
      const state = parseToleranceSpecJson(j);
      expect(toleranceEditorIsInert(state)).toBe(true);
      expect(composeToleranceBody(state)).toEqual({
        overall: { kind: "unspecified" },
        critical_features: [],
      });
    }
  });

  it("treats unparseable JSON as inert (deny-default, never throws)", () => {
    expect(toleranceEditorIsInert(parseToleranceSpecJson("{not json"))).toBe(
      true,
    );
  });
});

describe("toleranceEditorIsInert", () => {
  it("is false once the overall spec or a callout carries signal", () => {
    const withOverall = emptyToleranceEditorState();
    withOverall.overall.kind = "it_grade";
    expect(toleranceEditorIsInert(withOverall)).toBe(false);

    const withCallout = emptyToleranceEditorState();
    withCallout.criticalFeatures.push({
      featureIndex: "0",
      spec: { kind: "per_drawing", generalClass: "iso2768_medium", itGrade: "7", valueMm: "0.01" },
    });
    expect(toleranceEditorIsInert(withCallout)).toBe(false);
  });
});
