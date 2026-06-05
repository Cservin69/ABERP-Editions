// PR-242 / S250 finding 5 — vitest pin for the Workshop dashboard
// boundary normaliser. Asserts that a pre-v2.10.1 backend response
// (missing the S246 density-row arrays + `today_invoice_total`) is
// rounded to `[] / 0` defaults rather than left `undefined`. The pre-
// fix bug: `Workshop.svelte` accessed `dashboard.work_order_rows.length`
// directly; a mid-rollout SPA-vs-backend skew crashed the whole page
// with `TypeError: Cannot read properties of undefined (reading
// 'length')`. The boundary normaliser turns the unsafe shape into a
// safe one BEFORE any component renders against it.
//
// Wire shape contract: the six S246 row arrays AND `today_invoice_total`
// are optional on the type so a stale backend can omit them; the
// normaliser then back-fills `[]` / `0`. Component `?? []` guards stay
// as defense-in-depth at the use sites.

import { describe, expect, it } from "vitest";

import { __testHelpers, type WorkshopDashboard } from "./api";

// Minimum legal `WorkshopDashboard` shape WITHOUT the S246 fields.
// Mirrors the pre-v2.10.1 backend's response — every required field is
// present; the S246 additions are absent. The normaliser MUST return a
// shape where the missing keys are filled with their safe defaults.
const PRE_S246_PAYLOAD: WorkshopDashboard = {
  work_orders: {
    created: 0,
    released: 0,
    in_progress: 0,
    completed: 0,
    on_hold: 0,
    cancelled: 0,
  },
  low_stock_products: { count: 0 },
  qa: { pending: 0, passed: 0, failed: 0, reworking: 0, disposed: 0 },
  dispatch: {
    by_state: { drafted: 0, shipped: 0, cancelled: 0 },
    eligible_work_orders: 0,
    shipped_today: 0,
  },
  today: {
    date: "2026-06-05",
    issued_count_huf: 0,
    issued_count_eur: 0,
    gross_revenue_huf_minor: 0,
    gross_revenue_eur_minor: 0,
  },
  recent_activity: [],
  adapters: [],
  snapshot_at_iso8601: "2026-06-05T10:00:00.000Z",
};

describe("workshop-dashboard — safe-degrade against pre-S246 backend", () => {
  it("normalises missing density-row arrays to [] and missing total to 0", () => {
    const out = __testHelpers.normalizeWorkshopDashboard(PRE_S246_PAYLOAD);
    expect(out.work_order_rows).toEqual([]);
    expect(out.low_stock_rows).toEqual([]);
    expect(out.pending_qa_rows).toEqual([]);
    expect(out.eligible_dispatch_rows).toEqual([]);
    expect(out.pending_dispatch_rows).toEqual([]);
    expect(out.today_invoice_rows).toEqual([]);
    expect(out.today_invoice_total).toBe(0);
    // Aggregate counts still render — the underlying tile data is
    // preserved through the normaliser untouched.
    expect(out.work_orders.in_progress).toBe(0);
    expect(out.today.date).toBe("2026-06-05");
  });

  it("survives a payload whose row fields arrived as non-array junk", () => {
    // Defensive: SvelteKit's JSON parser turns explicit JSON `null` into
    // JS `null`, not `undefined` — and `null.length` throws too. The
    // Array.isArray guard catches every non-array shape.
    const junk = {
      ...PRE_S246_PAYLOAD,
      work_order_rows: null as unknown as never,
      today_invoice_total: null as unknown as never,
    } as WorkshopDashboard;
    const out = __testHelpers.normalizeWorkshopDashboard(junk);
    expect(out.work_order_rows).toEqual([]);
    expect(out.today_invoice_total).toBe(0);
  });

  it("preserves a fully-populated v2.10.1+ payload unchanged in shape", () => {
    const populated: WorkshopDashboard = {
      ...PRE_S246_PAYLOAD,
      work_order_rows: [
        {
          wo_id: "wo_01",
          wo_number: "WO-0001",
          product_name: "Widget",
          state: "in_progress",
          touched_at_iso8601: "2026-06-05T09:00:00Z",
          qty_target: "1",
        },
      ],
      today_invoice_total: 5,
      today_invoice_rows: [
        {
          invoice_id: "inv_01",
          sequence_number: 1,
          fiscal_year: 2026,
          currency: "HUF",
          total_gross_minor: 100000,
          buyer_name: "Acme Kft.",
          issue_date: "2026-06-05",
        },
      ],
    };
    const out = __testHelpers.normalizeWorkshopDashboard(populated);
    expect(out.work_order_rows?.length).toBe(1);
    expect(out.today_invoice_total).toBe(5);
    expect(out.today_invoice_rows?.[0].buyer_name).toBe("Acme Kft.");
  });
});
