// S6 / ADR-0094 Gap 3 — vitest pin for the gear-process helpers. Guards the
// pure TS mirror of the engine's `select_gear_process` (the live per-op
// preview) + the form composer, so a backend/engine drift surfaces here.

import { describe, expect, it } from "vitest";

import {
  composeGearProcessInputs,
  emptyGearProcessForm,
  selectGearProcess,
  GEAR_INTERNAL_WIRE_EDM_AGMA,
} from "./gear-processes";

describe("selectGearProcess (engine mirror)", () => {
  it("external → power-skive on a routed turning family, else hob", () => {
    expect(selectGearProcess("external_spur_helical", "swiss-turn-mill", 8)).toBe(
      "power_skive",
    );
    expect(selectGearProcess("external_spur_helical", "turn-mill", 8)).toBe(
      "power_skive",
    );
    expect(selectGearProcess("external_spur_helical", "3-axis-mill", 8)).toBe(
      "hob",
    );
    expect(selectGearProcess("external_spur_helical", "5-axis-mill", 8)).toBe(
      "hob",
    );
    expect(selectGearProcess("external_spur_helical", "lathe", 8)).toBe("hob");
  });

  it("internal → shape, escalating to wire-EDM strictly above AGMA 12", () => {
    expect(selectGearProcess("internal_ring", "3-axis-mill", 8)).toBe("shape");
    // exactly at the datum boundary stays shape (escalation is strict >)
    expect(
      selectGearProcess("internal_ring", "3-axis-mill", GEAR_INTERNAL_WIRE_EDM_AGMA),
    ).toBe("shape");
    expect(
      selectGearProcess(
        "internal_ring",
        "3-axis-mill",
        GEAR_INTERNAL_WIRE_EDM_AGMA + 1,
      ),
    ).toBe("wire_edm");
    // routed family is irrelevant for internal rings
    expect(selectGearProcess("internal_ring", "swiss-turn-mill", 14)).toBe(
      "wire_edm",
    );
  });
});

describe("composeGearProcessInputs", () => {
  it("parses the default form into the day-1 hob coefficients", () => {
    const body = composeGearProcessInputs(emptyGearProcessForm());
    expect(body).toEqual({
      process: "hob",
      setup_min: 20,
      min_per_tooth: 0.3,
      module_exponent: 1.0,
      agma_quality_factor_base: 0.1,
      in_cycle_factor: 1.0,
      notes: null,
    });
  });

  it("yields NaN for an unparseable numeric (backend rejects with a field error)", () => {
    const form = { ...emptyGearProcessForm(), minPerTooth: "abc" };
    expect(Number.isNaN(composeGearProcessInputs(form).min_per_tooth)).toBe(true);
  });
});
