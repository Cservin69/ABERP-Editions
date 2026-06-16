import { describe, it, expect } from "vitest";
import {
  allowedNextStates,
  capaPermitsClose,
  validateNcrDescription,
  splitList,
  SEVERITY_LABELS,
  CATEGORY_LABELS,
  STATE_LABELS,
  VERDICT_LABELS,
} from "./ncr";
import type { Capa, NcrState } from "./api";

describe("allowedNextStates", () => {
  it("mirrors the backend transition graph for every state", () => {
    expect(allowedNextStates("open")).toEqual([
      "contained",
      "under_investigation",
      "escalated",
    ]);
    expect(allowedNextStates("contained")).toEqual([
      "under_investigation",
      "escalated",
    ]);
    expect(allowedNextStates("under_investigation")).toEqual([
      "correction_applied",
      "escalated",
    ]);
    expect(allowedNextStates("correction_applied")).toEqual([
      "closed",
      "escalated",
    ]);
    expect(allowedNextStates("escalated")).toEqual([
      "under_investigation",
      "correction_applied",
      "closed",
    ]);
  });

  it("treats closed as terminal", () => {
    expect(allowedNextStates("closed")).toEqual([]);
  });

  it("never allows skipping straight to closed from open", () => {
    expect(allowedNextStates("open")).not.toContain("closed");
  });
});

function mkCapa(overrides: Partial<Capa>): Capa {
  return {
    capa_id: "capa_x",
    ncr_id: "ncr_x",
    corrective_action_text: "fix",
    preventive_action_text: "prevent",
    responsible_operator: "op",
    target_close_date: "2026-07-01",
    actual_close_date: null,
    effectiveness_review_at_utc: null,
    effectiveness_verdict: "pending",
    effectiveness_comment: null,
    approved_by_operator: null,
    approved_at_utc: null,
    created_at_utc: "2026-06-16T00:00:00Z",
    created_by_operator: "op",
    ...overrides,
  };
}

describe("capaPermitsClose", () => {
  it("requires both approval and a verified verdict", () => {
    expect(capaPermitsClose(mkCapa({}))).toBe(false);
    expect(
      capaPermitsClose(mkCapa({ approved_at_utc: "2026-06-16T01:00:00Z" })),
    ).toBe(false);
    expect(
      capaPermitsClose(
        mkCapa({ effectiveness_verdict: "verified" }),
      ),
    ).toBe(false);
    expect(
      capaPermitsClose(
        mkCapa({
          approved_at_utc: "2026-06-16T01:00:00Z",
          effectiveness_verdict: "verified",
        }),
      ),
    ).toBe(true);
    expect(
      capaPermitsClose(
        mkCapa({
          approved_at_utc: "2026-06-16T01:00:00Z",
          effectiveness_verdict: "not_effective",
        }),
      ),
    ).toBe(false);
  });
});

describe("validateNcrDescription", () => {
  it("rejects a blank description", () => {
    expect(validateNcrDescription("")).not.toBeNull();
    expect(validateNcrDescription("   ")).not.toBeNull();
  });
  it("rejects an over-long description", () => {
    expect(validateNcrDescription("x".repeat(4001))).not.toBeNull();
  });
  it("accepts a normal description", () => {
    expect(validateNcrDescription("bore out of tolerance")).toBeNull();
  });
});

describe("splitList", () => {
  it("splits on comma and newline, trimming + dropping blanks", () => {
    expect(splitList("dp-A, dp-B\n dp-C ,, \n")).toEqual([
      "dp-A",
      "dp-B",
      "dp-C",
    ]);
    expect(splitList("")).toEqual([]);
  });
});

describe("label maps", () => {
  it("cover every wire token", () => {
    expect(SEVERITY_LABELS.critical).toContain("Critical");
    expect(CATEGORY_LABELS.equipment_failure).toContain("Equipment");
    const states: NcrState[] = [
      "open",
      "contained",
      "under_investigation",
      "correction_applied",
      "closed",
      "escalated",
    ];
    for (const s of states) {
      expect(STATE_LABELS[s]).toBeTruthy();
    }
    expect(VERDICT_LABELS.verified).toContain("Verified");
  });
});
