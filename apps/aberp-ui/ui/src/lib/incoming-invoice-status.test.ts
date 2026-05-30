// PR-179 / session-179 — vitest pins for the IncomingInvoice status
// chip + allowed-action resolvers. Pure-helper coverage; one
// behaviour per assertion per CLAUDE.md rule 9.

import { describe, expect, it } from "vitest";

import {
  STATUS_META,
  actionsForStatus,
  metaForStatus,
} from "./incoming-invoice-status";

describe("incoming-invoice-status — metaForStatus", () => {
  it("maps Outstanding to amber chip with the wait glyph", () => {
    const m = metaForStatus("Outstanding");
    expect(m.cssClass).toBe("outstanding");
    expect(m.glyph).toBe("⌛");
    expect(m.label_hu).toBe("Kifizetésre vár");
    expect(m.label_en).toBe("Outstanding");
  });

  it("maps Paid to a green check", () => {
    const m = metaForStatus("Paid");
    expect(m.cssClass).toBe("paid");
    expect(m.glyph).toBe("✓");
    expect(m.label_en).toBe("Paid");
  });

  it("maps Irrelevant to a muted minus", () => {
    const m = metaForStatus("Irrelevant");
    expect(m.cssClass).toBe("irrelevant");
    expect(m.glyph).toBe("−");
    expect(m.label_en).toBe("Irrelevant");
  });

  it("returns an unknown chip for any other string (no throw, no coercion)", () => {
    const m = metaForStatus("Something-Else");
    expect(m.cssClass).toBe("unknown");
    expect(m.glyph).toBe("?");
  });

  it("STATUS_META covers exactly the three closed-vocab keys", () => {
    expect(Object.keys(STATUS_META).sort()).toEqual([
      "Irrelevant",
      "Outstanding",
      "Paid",
    ]);
  });
});

describe("incoming-invoice-status — actionsForStatus", () => {
  it("Outstanding offers mark-paid and mark-irrelevant only", () => {
    expect(actionsForStatus("Outstanding")).toEqual([
      "mark-paid",
      "mark-irrelevant",
    ]);
  });

  it("Paid offers mark-outstanding only (cannot cross directly to Irrelevant)", () => {
    expect(actionsForStatus("Paid")).toEqual(["mark-outstanding"]);
  });

  it("Irrelevant offers mark-outstanding only (cannot cross directly to Paid)", () => {
    expect(actionsForStatus("Irrelevant")).toEqual(["mark-outstanding"]);
  });

  it("returns no actions for an unrecognised status (deny-default)", () => {
    expect(actionsForStatus("Whatever")).toEqual([]);
  });
});
