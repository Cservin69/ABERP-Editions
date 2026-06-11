// S350 / PR-39 (U5) — tests for the operator material-grade override.
//
// Surfaces under test (all pure / shim-level — this package has no
// Svelte render harness, so the component wires straight to these and
// the gates are pinned here):
//   1. isMaterialEditable — the Edit-pencil visibility gate (editable
//      vs terminal/in-flight).
//   2. materialOptions — the catalogue→<option> mapping ("select
//      populated from catalogue mock").
//   3. parseMaterialEditError / editQuotePricingJobMaterial — the
//      Save → PATCH path: happy path + 400 (MaterialNotInCatalogue) +
//      409 (JobNotEditable) error typing.
//   4. materialEditInlineCopy — the bilingual inline message per code.

import { afterEach, describe, expect, it, vi } from "vitest";

import { invoke } from "@tauri-apps/api/core";
import {
  MaterialEditError,
  editQuotePricingJobMaterial,
  parseMaterialEditError,
  type QuotingMaterial,
} from "./api";
import {
  isMaterialEditable,
  materialEditInlineCopy,
  materialOptions,
} from "./pricing-material-edit";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

afterEach(() => {
  vi.mocked(invoke).mockReset();
});

function material(grade: string, displayName = ""): QuotingMaterial {
  return {
    grade,
    display_name: displayName,
    density_g_cm3: 2.7,
    cost_per_kg_eur: 5,
    machinability_index: 1,
    carbide_life_multiplier: 1,
    stock_status: "in_stock",
    lead_time_default_days: 3,
    quote_multiplier: 1,
    notes: null,
    updated_at: "2026-06-11T00:00:00Z",
    updated_by_actor: "test",
  };
}

describe("isMaterialEditable", () => {
  it("is true for the editable states (fetched / posting_back / failed)", () => {
    expect(isMaterialEditable("fetched")).toBe(true);
    expect(isMaterialEditable("posting_back")).toBe(true);
    expect(isMaterialEditable("failed")).toBe(true);
  });

  it("is false mid-pipeline and once posted (Edit pencil hidden)", () => {
    expect(isMaterialEditable("extracting")).toBe(false);
    expect(isMaterialEditable("pricing")).toBe(false);
    expect(isMaterialEditable("rendering")).toBe(false);
    expect(isMaterialEditable("posted")).toBe(false);
  });
});

describe("materialOptions", () => {
  it("maps the catalogue snapshot to value+label options, preserving order", () => {
    const opts = materialOptions([
      material("AL_6061_T6", "Alumínium 6061-T6"),
      material("SS_304"),
    ]);
    expect(opts).toEqual([
      { value: "AL_6061_T6", label: "Alumínium 6061-T6 (AL_6061_T6)" },
      { value: "SS_304", label: "SS_304" },
    ]);
  });

  it("returns an empty list for an empty catalogue", () => {
    expect(materialOptions([])).toEqual([]);
  });
});

describe("parseMaterialEditError", () => {
  it("types a 400 MaterialNotInCatalogue with its available_count", () => {
    const err = parseMaterialEditError(
      new Error(
        'backend returned 400 Bad Request for /api/quote-pricing-jobs/q: {"error":"MaterialNotInCatalogue","available_count":12}',
      ),
    );
    expect(err).toBeInstanceOf(MaterialEditError);
    expect(err.code).toBe("MaterialNotInCatalogue");
    expect(err.status).toBe(400);
    expect(err.availableCount).toBe(12);
  });

  it("types a 409 JobNotEditable", () => {
    const err = parseMaterialEditError(
      new Error(
        'backend returned 409 Conflict for /api/quote-pricing-jobs/q: {"error":"JobNotEditable","state":"posted","message":"nope"}',
      ),
    );
    expect(err.code).toBe("JobNotEditable");
    expect(err.status).toBe(409);
    expect(err.availableCount).toBeNull();
  });

  it("falls through to unknown on an unparseable error", () => {
    const err = parseMaterialEditError(new Error("network exploded"));
    expect(err.code).toBe("unknown");
    expect(err.status).toBe(0);
  });
});

describe("editQuotePricingJobMaterial shim", () => {
  it("forwards quoteId + materialGrade and returns the outcome verbatim", async () => {
    vi.mocked(invoke).mockResolvedValueOnce({
      quote_id: "q-1",
      old_grade: "unknown",
      new_grade: "AL_6061_T6",
      previous_state: "failed",
      new_attempt_n: 1,
    });
    const out = await editQuotePricingJobMaterial("q-1", "AL_6061_T6");
    expect(invoke).toHaveBeenCalledWith("edit_quote_pricing_job_material", {
      quoteId: "q-1",
      materialGrade: "AL_6061_T6",
    });
    expect(out.new_grade).toBe("AL_6061_T6");
    expect(out.previous_state).toBe("failed");
    expect(out.new_attempt_n).toBe(1);
  });

  it("rejects with a typed MaterialEditError on a 400 catalogue miss", async () => {
    vi.mocked(invoke).mockRejectedValueOnce(
      new Error(
        'backend returned 400 Bad Request for /api/quote-pricing-jobs/q: {"error":"MaterialNotInCatalogue","available_count":7}',
      ),
    );
    await expect(
      editQuotePricingJobMaterial("q-1", "bogus"),
    ).rejects.toMatchObject({ code: "MaterialNotInCatalogue", availableCount: 7 });
  });

  it("rejects with a typed MaterialEditError on a 409 terminal row", async () => {
    vi.mocked(invoke).mockRejectedValueOnce(
      new Error(
        'backend returned 409 Conflict for /api/quote-pricing-jobs/q: {"error":"JobNotEditable","state":"posted"}',
      ),
    );
    await expect(
      editQuotePricingJobMaterial("q-1", "AL_6061_T6"),
    ).rejects.toMatchObject({ code: "JobNotEditable", status: 409 });
  });
});

describe("materialEditInlineCopy", () => {
  it("catalogue-miss copy is bilingual and names the available count", () => {
    const copy = materialEditInlineCopy(
      new MaterialEditError("MaterialNotInCatalogue", 400, "x", 9),
    );
    expect(copy).toContain("nincs a katalógusban");
    expect(copy).toContain("not in the catalogue");
    expect(copy).toContain("9");
  });

  it("terminal-state copy explains why the edit is refused, bilingually", () => {
    const copy = materialEditInlineCopy(
      new MaterialEditError("JobNotEditable", 409, "x"),
    );
    expect(copy).toContain("nem szerkeszthető");
    expect(copy).toContain("cannot be edited");
  });

  it("unknown code falls back to the raw message", () => {
    const copy = materialEditInlineCopy(
      new MaterialEditError("unknown", 0, "raw boom"),
    );
    expect(copy).toBe("raw boom");
  });
});
