// S427 — vitest pins for the machines helper module. Composer +
// mapper + filter + lead-time helpers + typed-error parser are pure
// functions (no Svelte, no Tauri); pinning them in isolation surfaces
// regressions before the dev-loop renders the form. Mirror of
// `partners.test.ts`.

import { describe, expect, it } from "vitest";

import type { QuotingMachine } from "./api";
import {
  EMPTY_MACHINE_FILTER,
  composeMachineInputs,
  effectiveLeadTime,
  emptyMachineForm,
  filterMachines,
  formFromMachine,
  isMachineFilterEmpty,
  leadTimeChipClass,
  machineFamilyLabel,
  parseMachineValidationError,
} from "./machines";

const SAMPLE_MACHINE: QuotingMachine = {
  id: "qcm_01ARZ3NDEKTSV4RRFFQ69G5FAV",
  name: "DMG MORI DMU 50",
  family: "5-axis-mill",
  max_envelope_xyz_mm: [500, 450, 400],
  daily_hours_avail: 16,
  buffer_pct: 20,
  enabled: true,
  created_at: "2026-06-15T08:00:00Z",
  updated_at: "2026-06-15T08:00:00Z",
  archived_at: null,
};

describe("emptyMachineForm", () => {
  it("defaults family=3-axis-mill, 16h, 20% buffer, enabled, 0 envelope", () => {
    const form = emptyMachineForm();
    expect(form.family).toBe("3-axis-mill");
    expect(form.dailyHoursAvail).toBe("16");
    expect(form.bufferPct).toBe("20");
    expect(form.enabled).toBe(true);
    expect(form.envelopeX).toBe("0");
    expect(form.envelopeY).toBe("0");
    expect(form.envelopeZ).toBe("0");
    expect(form.name).toBe("");
  });
});

describe("formFromMachine", () => {
  it("round-trips every field one-to-one (numbers stringified)", () => {
    const form = formFromMachine(SAMPLE_MACHINE);
    expect(form.name).toBe("DMG MORI DMU 50");
    expect(form.family).toBe("5-axis-mill");
    expect(form.envelopeX).toBe("500");
    expect(form.envelopeY).toBe("450");
    expect(form.envelopeZ).toBe("400");
    expect(form.dailyHoursAvail).toBe("16");
    expect(form.bufferPct).toBe("20");
    expect(form.enabled).toBe(true);
  });

  it("preserves enabled=false verbatim", () => {
    const form = formFromMachine({ ...SAMPLE_MACHINE, enabled: false });
    expect(form.enabled).toBe(false);
  });
});

describe("composeMachineInputs", () => {
  it("trims the name", () => {
    const body = composeMachineInputs({
      ...emptyMachineForm(),
      name: "  Haas VF-2  ",
    });
    expect(body.name).toBe("Haas VF-2");
  });

  it("parses numeric fields via parseFloat (envelope triple + hours + buffer)", () => {
    const body = composeMachineInputs({
      ...emptyMachineForm(),
      name: "X",
      envelopeX: "300.5",
      envelopeY: "250",
      envelopeZ: "200.25",
      dailyHoursAvail: "8",
      bufferPct: "15",
    });
    expect(body.max_envelope_xyz_mm).toEqual([300.5, 250, 200.25]);
    expect(body.daily_hours_avail).toBe(8);
    expect(body.buffer_pct).toBe(15);
  });

  it("emits snake_case wire field names (no camelCase leak)", () => {
    const body = composeMachineInputs({
      ...emptyMachineForm(),
      name: "X",
    });
    expect("max_envelope_xyz_mm" in body).toBe(true);
    expect("daily_hours_avail" in body).toBe(true);
    expect("buffer_pct" in body).toBe(true);
    // camelCase form-state keys must NOT leak onto the wire.
    expect("dailyHoursAvail" in body).toBe(false);
    expect("bufferPct" in body).toBe(false);
    expect("envelopeX" in body).toBe(false);
  });

  it("preserves family + enabled verbatim", () => {
    const body = composeMachineInputs({
      ...emptyMachineForm(),
      name: "X",
      family: "wire-EDM",
      enabled: false,
    });
    expect(body.family).toBe("wire-EDM");
    expect(body.enabled).toBe(false);
  });

  it("composes a round-trip from formFromMachine back to the wire shape", () => {
    const body = composeMachineInputs(formFromMachine(SAMPLE_MACHINE));
    expect(body.name).toBe(SAMPLE_MACHINE.name);
    expect(body.family).toBe(SAMPLE_MACHINE.family);
    expect(body.max_envelope_xyz_mm).toEqual(
      SAMPLE_MACHINE.max_envelope_xyz_mm,
    );
    expect(body.daily_hours_avail).toBe(SAMPLE_MACHINE.daily_hours_avail);
    expect(body.buffer_pct).toBe(SAMPLE_MACHINE.buffer_pct);
    expect(body.enabled).toBe(SAMPLE_MACHINE.enabled);
  });
});

describe("machineFamilyLabel", () => {
  it("maps known db-strings to human labels", () => {
    expect(machineFamilyLabel("3-axis-mill")).toBe("3-axis mill");
    expect(machineFamilyLabel("wire-EDM")).toBe("Wire EDM");
  });

  it("falls back to the raw string for an unknown family", () => {
    expect(machineFamilyLabel("plasma-cutter")).toBe("plasma-cutter");
  });
});

describe("leadTimeChipClass — boundaries", () => {
  it("<= 7 days → ok", () => {
    expect(leadTimeChipClass(0)).toBe("chip chip--ok");
    expect(leadTimeChipClass(7)).toBe("chip chip--ok");
  });

  it("8..21 days → warning", () => {
    expect(leadTimeChipClass(8)).toBe("chip chip--warning");
    expect(leadTimeChipClass(21)).toBe("chip chip--warning");
  });

  it("> 21 days → err", () => {
    expect(leadTimeChipClass(22)).toBe("chip chip--err");
    expect(leadTimeChipClass(60)).toBe("chip chip--err");
  });
});

describe("effectiveLeadTime", () => {
  it("prefers the override over the computed value", () => {
    expect(effectiveLeadTime(10, 3)).toBe(3);
  });

  it("falls back to computed when the override is null", () => {
    expect(effectiveLeadTime(10, null)).toBe(10);
  });

  it("returns null when both are null", () => {
    expect(effectiveLeadTime(null, null)).toBeNull();
  });

  it("treats a zero override as a real value (not falsy fallback)", () => {
    // 0 is a legitimate same-day override; `??` must NOT fall through.
    expect(effectiveLeadTime(10, 0)).toBe(0);
  });
});

describe("filterMachines", () => {
  const rows: QuotingMachine[] = [
    { ...SAMPLE_MACHINE, id: "qcm_a", name: "Haas VF-2", family: "3-axis-mill" },
    { ...SAMPLE_MACHINE, id: "qcm_b", name: "DMU 50", family: "5-axis-mill" },
    { ...SAMPLE_MACHINE, id: "qcm_c", name: "Fanuc Robocut", family: "wire-EDM" },
  ];

  it("EMPTY_MACHINE_FILTER passes every row", () => {
    expect(filterMachines(rows, EMPTY_MACHINE_FILTER)).toEqual(rows);
    expect(isMachineFilterEmpty(EMPTY_MACHINE_FILTER)).toBe(true);
  });

  it("filters case-insensitively on name", () => {
    expect(filterMachines(rows, { needle: "haas", family: "All" })).toEqual([
      rows[0],
    ]);
    expect(filterMachines(rows, { needle: "ROBOCUT", family: "All" })).toEqual([
      rows[2],
    ]);
  });

  it("family facet AND-composes with the needle", () => {
    const out = filterMachines(rows, { needle: "dmu", family: "5-axis-mill" });
    expect(out.map((m) => m.id)).toEqual(["qcm_b"]);
  });

  it("family facet alone gates without a needle", () => {
    const out = filterMachines(rows, { needle: "", family: "wire-EDM" });
    expect(out.map((m) => m.id)).toEqual(["qcm_c"]);
  });

  it("isMachineFilterEmpty is false when the family facet is engaged", () => {
    expect(isMachineFilterEmpty({ needle: "", family: "lathe" })).toBe(false);
  });
});

describe("parseMachineValidationError", () => {
  it("extracts the typed body from the Tauri-wrapped error string", () => {
    const raw =
      "backend returned 400 Bad Request for /api/machines: " +
      '{"error":"validation_failed","fields":[' +
      '{"field":"name","message":"name is required"},' +
      '{"field":"max_envelope_xyz_mm","message":"envelope must be positive"}' +
      "]}";
    const parsed = parseMachineValidationError(raw);
    expect(parsed).not.toBeNull();
    expect(parsed!.fields.length).toBe(2);
    expect(parsed!.fields[0].field).toBe("name");
    expect(parsed!.fields[1].field).toBe("max_envelope_xyz_mm");
  });

  it("returns null for a malformed body", () => {
    expect(parseMachineValidationError("network error")).toBeNull();
    expect(
      parseMachineValidationError("backend returned 500 ISE: <html>"),
    ).toBeNull();
  });

  it("returns null when the discriminant is wrong", () => {
    const raw =
      'backend returned 404 for /api/machines/x: {"error":"machine not found"}';
    expect(parseMachineValidationError(raw)).toBeNull();
  });
});
