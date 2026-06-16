// S440 (ADR-0068) — pins for the pure Purchasing helpers. These mirror the
// backend invariants (`aberp::purchasing`) so the SPA's instant feedback can
// never disagree with the authoritative POST-route re-validation.

import { describe, it, expect } from "vitest";
import {
  allowedNextStates,
  avlChip,
  formatPoMoney,
  issueBlockedByPending,
  lineRemaining,
  lineTotalMinor,
  poTotals,
  validateNewPo,
} from "./purchasing";
import type { NewPoLineInput, PoLine } from "./api";

describe("allowedNextStates — mirrors backend allowed_transition", () => {
  it("draft can issue or cancel", () => {
    expect(allowedNextStates("draft")).toEqual(["issued_to_vendor", "cancelled"]);
  });
  it("issued can only be cancelled by the operator (receipts drive the rest)", () => {
    expect(allowedNextStates("issued_to_vendor")).toEqual(["cancelled"]);
  });
  it("partially_received can be cancelled", () => {
    expect(allowedNextStates("partially_received")).toEqual(["cancelled"]);
  });
  it("received can be closed", () => {
    expect(allowedNextStates("received")).toEqual(["closed"]);
  });
  it("closed + cancelled are terminal", () => {
    expect(allowedNextStates("closed")).toEqual([]);
    expect(allowedNextStates("cancelled")).toEqual([]);
  });
  it("never offers the receipt-driven states as operator targets", () => {
    for (const s of ["draft", "issued_to_vendor", "partially_received", "received"] as const) {
      expect(allowedNextStates(s)).not.toContain("partially_received");
      expect(allowedNextStates(s)).not.toContain("received");
    }
  });
});

describe("money math", () => {
  it("line total multiplies, guarding overflow", () => {
    expect(lineTotalMinor(3, 1500)).toBe(4500);
    expect(lineTotalMinor(Number.MAX_SAFE_INTEGER, 2)).toBeNull();
  });
  it("PO totals roll up subtotal + floored VAT", () => {
    const lines = [
      { quantity: 2, unit_price_minor: 5000 },
      { quantity: 1, unit_price_minor: 1000 },
    ];
    expect(poTotals(lines, 27)).toEqual({
      subtotalMinor: 11000,
      vatMinor: 2970,
      totalMinor: 13970,
    });
  });
  it("VAT floors", () => {
    expect(poTotals([{ quantity: 1, unit_price_minor: 101 }], 27).vatMinor).toBe(27);
  });
  it("zero VAT is a no-op", () => {
    expect(poTotals([{ quantity: 1, unit_price_minor: 100 }], 0)).toEqual({
      subtotalMinor: 100,
      vatMinor: 0,
      totalMinor: 100,
    });
  });
});

describe("formatPoMoney", () => {
  it("HUF renders whole (zero-decimal)", () => {
    expect(formatPoMoney(12000, "HUF").replace(/\s/g, " ")).toBe("12 000 HUF");
  });
  it("USD renders 2 decimals", () => {
    expect(formatPoMoney(12345, "usd")).toBe("123.45 USD");
  });
  it("EUR renders 2 decimals with padded cents", () => {
    expect(formatPoMoney(1005, "EUR")).toBe("10.05 EUR");
  });
});

describe("lineRemaining", () => {
  const line = (q: number, r: number): PoLine => ({
    pol_id: "pol_x",
    po_id: "po_x",
    product_id: null,
    description: "bar",
    quantity: q,
    unit_price_minor: 100,
    currency: "HUF",
    line_total_minor: q * 100,
    expected_heat_lot_required: false,
    received_quantity: r,
  });
  it("is quantity minus received, floored at 0", () => {
    expect(lineRemaining(line(5, 2))).toBe(3);
    expect(lineRemaining(line(5, 5))).toBe(0);
    expect(lineRemaining(line(5, 6))).toBe(0);
  });
});

describe("avlChip", () => {
  it("approved → green, conditional → yellow", () => {
    expect(avlChip("approved")?.tone).toBe("green");
    expect(avlChip("conditional")?.tone).toBe("yellow");
  });
  it("pending → grey, suspended/revoked → red", () => {
    expect(avlChip("pending")?.tone).toBe("grey");
    expect(avlChip("suspended")?.tone).toBe("red");
    expect(avlChip("revoked")?.tone).toBe("red");
  });
  it("unlisted vendor → no chip", () => {
    expect(avlChip(null)).toBeNull();
  });
  it("only pending blocks issue client-side", () => {
    expect(issueBlockedByPending("pending")).toBe(true);
    expect(issueBlockedByPending("approved")).toBe(false);
    expect(issueBlockedByPending("conditional")).toBe(false);
    expect(issueBlockedByPending(null)).toBe(false);
  });
});

describe("validateNewPo", () => {
  const okLine: NewPoLineInput = {
    description: "316L bar",
    quantity: 10,
    unit_price_minor: 2500,
    expected_heat_lot_required: true,
  };
  it("accepts a well-formed PO", () => {
    expect(
      validateNewPo({
        vendor_partner_id: "ptn_1",
        currency: "EUR",
        vat_rate_pct: 27,
        lines: [okLine],
      }),
    ).toEqual([]);
  });
  it("rejects missing vendor, bad currency, bad VAT, empty lines", () => {
    const errs = validateNewPo({
      vendor_partner_id: "  ",
      currency: "EURO",
      vat_rate_pct: 200,
      lines: [],
    });
    expect(errs.length).toBe(4);
  });
  it("rejects a blank-description / non-positive-quantity line", () => {
    const errs = validateNewPo({
      vendor_partner_id: "ptn_1",
      currency: "HUF",
      vat_rate_pct: 0,
      lines: [{ description: " ", quantity: 0, unit_price_minor: -1, expected_heat_lot_required: false }],
    });
    expect(errs.length).toBe(3);
  });
});
