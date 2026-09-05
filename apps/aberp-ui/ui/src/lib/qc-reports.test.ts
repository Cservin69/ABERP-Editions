import { describe, it, expect } from "vitest";
import {
  draftTokenForKind,
  templatePermitsKind,
  templatesForKind,
  DRAFTABLE_KINDS,
  reportKindLabel,
  stateTone,
  dispositionTone,
  permitsShipment,
  accountabilityLabel,
  toleranceLabel,
  actualLabel,
  emptyDraftForm,
  composeDraftBody,
  validateDraftForm,
  validateVoidReason,
  canIssue,
  canVoid,
  canRenderPdf,
  filterReports,
  qcErrorMessage,
} from "./qc-reports";
import type { QcReport } from "./api";

// The one load-bearing vocab asymmetry: a CoC reads back as
// "certificate_of_conformance" but is drafted with the token "coc".
describe("CoC wire/storage token split", () => {
  it("maps the CoC wire kind to the 'coc' draft token", () => {
    expect(draftTokenForKind("certificate_of_conformance")).toBe("coc");
  });
  it("passes the other kinds through unchanged", () => {
    expect(draftTokenForKind("dimensional_inspection")).toBe("dimensional_inspection");
    expect(draftTokenForKind("as9102_fair")).toBe("as9102_fair");
  });
  it("DRAFTABLE_KINDS pairs the 'coc' input with the CoC wire kind", () => {
    const coc = DRAFTABLE_KINDS.find((k) => k.input === "coc");
    expect(coc?.wire).toBe("certificate_of_conformance");
    // Every triple's draftTokenForKind(wire) round-trips to its input token.
    for (const k of DRAFTABLE_KINDS) {
      expect(draftTokenForKind(k.wire)).toBe(k.input);
    }
  });
});

describe("kind↔template compatibility mirrors the backend permits()", () => {
  it("ÁBEN standard produces dimensional + CoC, but not a FAIR", () => {
    expect(templatePermitsKind("aben_standard", "dimensional_inspection")).toBe(true);
    expect(templatePermitsKind("aben_standard", "coc")).toBe(true);
    expect(templatePermitsKind("aben_standard", "as9102_fair")).toBe(false);
  });
  it("AS9102 Rev C produces every kind; CoC-only produces only a CoC", () => {
    expect(templatePermitsKind("as9102_rev_c", "as9102_fair")).toBe(true);
    expect(templatePermitsKind("as9102_rev_c", "dimensional_inspection")).toBe(true);
    expect(templatePermitsKind("coc_only", "coc")).toBe(true);
    expect(templatePermitsKind("coc_only", "dimensional_inspection")).toBe(false);
  });
  it("templatesForKind lists only the compatible templates", () => {
    expect(templatesForKind("as9102_fair")).toEqual(["as9102_rev_c"]);
    expect(templatesForKind("dimensional_inspection")).toEqual([
      "aben_standard",
      "as9102_rev_c",
    ]);
    expect(templatesForKind("coc")).toEqual([
      "aben_standard",
      "as9102_rev_c",
      "coc_only",
    ]);
  });
});

describe("shipment rule mirrors the backend", () => {
  it("accept and accept_with_ncr permit shipment; reject and incomplete do not", () => {
    expect(permitsShipment("accept")).toBe(true);
    expect(permitsShipment("accept_with_ncr")).toBe(true);
    expect(permitsShipment("reject")).toBe(false);
    expect(permitsShipment("incomplete")).toBe(false);
  });
});

describe("report-line formatters never fabricate a value", () => {
  it("shows an explicit not-measured blank rather than a zero", () => {
    expect(
      actualLabel({ actual_value: null, accountability: "not_measured", units: "mm" }),
    ).toBe("— (not measured)");
  });
  it("renders N/A for a not-applicable characteristic", () => {
    expect(
      actualLabel({ actual_value: null, accountability: "not_applicable", units: null }),
    ).toBe("N/A");
  });
  it("appends units to a measured actual", () => {
    expect(
      actualLabel({ actual_value: 12.51, accountability: "measured", units: "mm" }),
    ).toBe("12.51 mm");
  });
  it("formats a tolerance band and falls back to a dash for a no-nominal line", () => {
    expect(
      toleranceLabel({ nominal_value: 12.5, upper_tol: 0.05, lower_tol: -0.02, units: "mm" }),
    ).toBe("12.5 +0.05 / -0.02 mm");
    expect(
      toleranceLabel({ nominal_value: null, upper_tol: null, lower_tol: null, units: null }),
    ).toBe("—");
  });
  it("labels accountability states", () => {
    expect(accountabilityLabel("measured")).toBe("Measured");
    expect(accountabilityLabel("not_measured")).toBe("Not measured");
    expect(accountabilityLabel("not_applicable")).toBe("N/A");
  });
});

describe("draft composer", () => {
  it("omits template and notes when blank so the backend resolves defaults", () => {
    const form = emptyDraftForm("wo_1");
    expect(composeDraftBody(form)).toEqual({
      wo_id: "wo_1",
      report_kind: "dimensional_inspection",
    });
  });
  it("carries a chosen template + trimmed notes and the coc token", () => {
    const body = composeDraftBody({
      woId: "  wo_2 ",
      kind: "coc",
      template: "as9102_rev_c",
      notes: "  first article  ",
    });
    expect(body).toEqual({
      wo_id: "wo_2",
      report_kind: "coc",
      template: "as9102_rev_c",
      notes: "first article",
    });
  });
  it("requires a work order", () => {
    expect(validateDraftForm(emptyDraftForm(""))).toHaveProperty("wo_id");
    expect(validateDraftForm(emptyDraftForm("wo_1"))).toEqual({});
  });
});

describe("void validation", () => {
  it("requires a non-blank reason", () => {
    expect(validateVoidReason("   ")).toHaveProperty("reason");
    expect(validateVoidReason("wrong revision")).toEqual({});
  });
});

describe("lifecycle predicates gate the detail-pane actions", () => {
  it("only a drafted report can be issued", () => {
    expect(canIssue({ state: "drafted" })).toBe(true);
    expect(canIssue({ state: "issued" })).toBe(false);
    expect(canIssue({ state: "voided" })).toBe(false);
  });
  it("drafted or issued can be voided; voided/superseded cannot", () => {
    expect(canVoid({ state: "drafted" })).toBe(true);
    expect(canVoid({ state: "issued" })).toBe(true);
    expect(canVoid({ state: "voided" })).toBe(false);
    expect(canVoid({ state: "superseded" })).toBe(false);
  });
  it("only a current report renders a PDF (voided/superseded → no document)", () => {
    expect(canRenderPdf({ state: "issued" })).toBe(true);
    expect(canRenderPdf({ state: "drafted" })).toBe(true);
    expect(canRenderPdf({ state: "voided" })).toBe(false);
    expect(canRenderPdf({ state: "superseded" })).toBe(false);
  });
});

describe("tones + labels", () => {
  it("voided is danger, issued is positive, incomplete is a warning", () => {
    expect(stateTone("voided")).toBe("danger");
    expect(stateTone("issued")).toBe("positive");
    expect(dispositionTone("incomplete")).toBe("warning");
    expect(dispositionTone("reject")).toBe("danger");
  });
  it("labels the CoC kind", () => {
    expect(reportKindLabel("certificate_of_conformance")).toBe("Certificate of Conformance");
  });
});

describe("list filter", () => {
  const rows = [
    { report_number: "QCR-2026-0007", report_kind: "as9102_fair", wo_id: "wo_alpha" },
    { report_number: "QCR-2026-0008", report_kind: "certificate_of_conformance", wo_id: "wo_beta" },
  ] as QcReport[];
  it("matches on report number, kind label, and wo id; empty needle is identity", () => {
    expect(filterReports(rows, "0007")).toHaveLength(1);
    expect(filterReports(rows, "conformance")).toHaveLength(1);
    expect(filterReports(rows, "wo_")).toHaveLength(2);
    expect(filterReports(rows, "  ")).toHaveLength(2);
    expect(filterReports(rows, "nope")).toHaveLength(0);
  });
});

describe("error message extraction", () => {
  it("peels a message out of a wrapped JSON error body", () => {
    expect(qcErrorMessage('Error: {"error":"validation","message":"reason is required"}')).toBe(
      "reason is required",
    );
  });
  it("falls back to the raw string when there is no JSON", () => {
    expect(qcErrorMessage("plain transport failure")).toBe("plain transport failure");
  });
});
