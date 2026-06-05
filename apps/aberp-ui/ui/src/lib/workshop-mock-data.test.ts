// S238 / PR-232 — vitest pin for the Workshop demo-mode mock
// payload. The point is to catch backend widenings (a new required
// field on `WorkshopDashboard`) at vitest time rather than at
// render time. A property that becomes required on the type but
// missing from the mock will fail this file's TS compile.

import { describe, expect, it } from "vitest";

import type { WorkshopDashboard } from "./api";
import {
  MOCK_SCAN_MESSAGES,
  MOCK_SPOTLIGHT_TILES,
  getMockDashboard,
} from "./workshop-mock-data";

describe("workshop-mock-data — shape pin", () => {
  it("getMockDashboard returns a value assignable to WorkshopDashboard", () => {
    // The type annotation is the assertion — TS would reject the
    // file at build time if `getMockDashboard` ever drifted from
    // the real wire shape. Runtime checks then pin the realistic
    // ranges per the brief.
    const b: WorkshopDashboard = getMockDashboard(
      new Date("2026-06-04T10:30:00.000Z"),
    );
    expect(b.snapshot_at_iso8601).toBe("2026-06-04T10:30:00.000Z");
    expect(b.today.date).toBe("2026-06-04");
  });

  it("work-order counts sum to 18 (brief target: realistic Q2 mid-day load)", () => {
    const b = getMockDashboard();
    const total =
      b.work_orders.created +
      b.work_orders.released +
      b.work_orders.in_progress +
      b.work_orders.on_hold +
      b.work_orders.completed +
      b.work_orders.cancelled;
    expect(total).toBe(18);
  });

  it("low-stock count sits in the 'watch list' band (1-5)", () => {
    const b = getMockDashboard();
    // Per the brief: "3 products below min (not 0, not 20 —
    // 'watch list' feel)". Pin the band so a future tweak that
    // overshoots the realism target trips this test.
    expect(b.low_stock_products.count).toBeGreaterThan(0);
    expect(b.low_stock_products.count).toBeLessThan(6);
  });

  it("QA pending+reworking is operator-actionable but not overloaded", () => {
    const b = getMockDashboard();
    expect(b.qa.pending).toBe(6);
    expect(b.qa.reworking).toBe(1);
    expect(b.qa.failed).toBe(0);
    expect(b.qa.disposed).toBe(0);
  });

  it("dispatch numbers tell a 'morning work cleared, afternoon queued' story", () => {
    const b = getMockDashboard();
    expect(b.dispatch.shipped_today).toBe(3);
    expect(b.dispatch.eligible_work_orders).toBe(4);
    expect(b.dispatch.by_state.drafted).toBe(2);
  });

  it("today panel reports 8 invoices and meaningful gross revenue (HUF+EUR mix)", () => {
    const b = getMockDashboard();
    expect(b.today.issued_count_huf + b.today.issued_count_eur).toBe(8);
    expect(b.today.gross_revenue_huf_minor).toBe(428_740_000); // 4,287,400 HUF
    expect(b.today.gross_revenue_eur_minor).toBe(1_245_000); // 12,450 EUR
  });

  it("all adapters are healthy — shop floor 'green' demo state (S240 vocab)", () => {
    const b = getMockDashboard();
    expect(b.adapters.length).toBeGreaterThan(0);
    for (const a of b.adapters) {
      expect(a.status).toBe("healthy");
    }
  });

  it("recent activity has 15-20 entries spanning the last 90 minutes", () => {
    const now = new Date("2026-06-04T12:00:00.000Z");
    const b = getMockDashboard(now);
    expect(b.recent_activity.length).toBeGreaterThanOrEqual(15);
    expect(b.recent_activity.length).toBeLessThanOrEqual(20);
    const baseMs = now.getTime();
    for (const entry of b.recent_activity) {
      const t = new Date(entry.at_iso8601).getTime();
      expect(t).toBeLessThanOrEqual(baseMs);
      // No entry older than 90 minutes (5400s).
      expect(baseMs - t).toBeLessThanOrEqual(90 * 60 * 1000);
    }
  });

  it("recent activity is newest-first (descending by timestamp)", () => {
    const b = getMockDashboard();
    for (let i = 1; i < b.recent_activity.length; i++) {
      const prev = new Date(b.recent_activity[i - 1].at_iso8601).getTime();
      const cur = new Date(b.recent_activity[i].at_iso8601).getTime();
      expect(prev).toBeGreaterThanOrEqual(cur);
    }
  });

  it("recent activity entries have unique ids and monotonically-decreasing seq", () => {
    const b = getMockDashboard();
    const ids = new Set<string>();
    for (const e of b.recent_activity) ids.add(e.id);
    expect(ids.size).toBe(b.recent_activity.length);
    // Newest first means seq decreases as we walk forward.
    for (let i = 1; i < b.recent_activity.length; i++) {
      expect(b.recent_activity[i].seq).toBeLessThan(b.recent_activity[i - 1].seq);
    }
  });
});

describe("workshop-mock-data — S246 / PR-239 density rows", () => {
  it("work_order_rows has exactly 5 rows spanning the snake_case state vocab", () => {
    const b = getMockDashboard();
    expect(b.work_order_rows.length).toBe(5);
    const states = new Set(b.work_order_rows.map((r) => r.state));
    // Each state is one of the documented snake_case wire forms.
    for (const s of states) {
      expect([
        "created",
        "released",
        "in_progress",
        "completed",
        "cancelled",
        "on_hold",
      ]).toContain(s);
    }
    // WO numbers follow the `WO-YYYY-NNNNN` shape.
    for (const r of b.work_order_rows) {
      expect(r.wo_number).toMatch(/^WO-\d{4}-\d{5}$/);
      expect(r.product_name).not.toBe("");
    }
  });

  it("low_stock_rows has exactly 3 rows with bin-location grid codes", () => {
    const b = getMockDashboard();
    expect(b.low_stock_rows.length).toBe(3);
    for (const r of b.low_stock_rows) {
      expect(r.bin_location).toMatch(/^[A-Z]-\d{2}-\d$/);
      // stock < min by construction — this is the low-stock list.
      expect(Number(r.stock_qty)).toBeLessThan(Number(r.min_stock));
    }
  });

  it("pending_qa_rows has exactly 7 rows oldest-first", () => {
    const now = new Date("2026-06-04T12:00:00.000Z");
    const b = getMockDashboard(now);
    expect(b.pending_qa_rows.length).toBe(7);
    for (let i = 1; i < b.pending_qa_rows.length; i++) {
      const prev = new Date(b.pending_qa_rows[i - 1].created_at_iso8601).getTime();
      const cur = new Date(b.pending_qa_rows[i].created_at_iso8601).getTime();
      // Oldest first per ADR-0063 §8 — created_at is ascending.
      expect(prev).toBeLessThanOrEqual(cur);
    }
  });

  it("eligible_dispatch_rows has exactly 4 rows oldest-first", () => {
    const b = getMockDashboard(new Date("2026-06-04T12:00:00.000Z"));
    expect(b.eligible_dispatch_rows.length).toBe(4);
    for (let i = 1; i < b.eligible_dispatch_rows.length; i++) {
      const prev = new Date(
        b.eligible_dispatch_rows[i - 1].completed_at_iso8601,
      ).getTime();
      const cur = new Date(b.eligible_dispatch_rows[i].completed_at_iso8601).getTime();
      // Older WOs head the list per ADR-0064.
      expect(prev).toBeLessThanOrEqual(cur);
    }
  });

  it("pending_dispatch_rows has exactly 2 rows newest-first with partner names", () => {
    const b = getMockDashboard(new Date("2026-06-04T12:00:00.000Z"));
    expect(b.pending_dispatch_rows.length).toBe(2);
    const a = new Date(b.pending_dispatch_rows[0].created_at_iso8601).getTime();
    const c = new Date(b.pending_dispatch_rows[1].created_at_iso8601).getTime();
    // Newest first per ADR-0064 §7 — created_at is descending.
    expect(a).toBeGreaterThanOrEqual(c);
    for (const r of b.pending_dispatch_rows) {
      expect(r.partner_name).not.toBe("");
    }
  });

  it("today_invoice_rows has exactly 5 rows + total=8 (so '+3 more' surfaces)", () => {
    const b = getMockDashboard();
    expect(b.today_invoice_rows.length).toBe(5);
    expect(b.today_invoice_total).toBe(8);
    expect(b.today_invoice_total).toBeGreaterThan(b.today_invoice_rows.length);
    // Sequence-number descending — newest issuance heads the list.
    for (let i = 1; i < b.today_invoice_rows.length; i++) {
      expect(b.today_invoice_rows[i].sequence_number).toBeLessThan(
        b.today_invoice_rows[i - 1].sequence_number,
      );
    }
    // Currency split — at least one EUR row so the formatter
    // dual-symbol path renders in demo.
    const huf = b.today_invoice_rows.filter((r) => r.currency === "HUF").length;
    const eur = b.today_invoice_rows.filter((r) => r.currency === "EUR").length;
    expect(huf).toBeGreaterThan(0);
    expect(eur).toBeGreaterThan(0);
  });
});

describe("workshop-mock-data — tickers", () => {
  it("MOCK_SCAN_MESSAGES is non-empty so the rotation has something to show", () => {
    expect(MOCK_SCAN_MESSAGES.length).toBeGreaterThan(0);
  });

  it("MOCK_SPOTLIGHT_TILES covers all seven tile testids", () => {
    const expected = [
      "tile-work-orders",
      "tile-qa",
      "tile-dispatch",
      "tile-adapters",
      "tile-low-stock",
      "tile-today",
      "tile-recent-activity",
    ];
    // Same set, order-independent — the rotation order is a
    // theatre choice that may evolve without breaking this pin.
    expect([...MOCK_SPOTLIGHT_TILES].sort()).toEqual(expected.sort());
  });
});
