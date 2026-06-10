import { describe, expect, test, vi } from "vitest";

import type { CataloguePushStatus, QuotingMaterial } from "./api";
import {
  compareMaterials,
  renderPushStatusSuffix,
  sortMaterials,
  STOCK_STATUS_ORDER,
  stockStatusLabel,
  stockStatusTone,
  toggleSort,
  type SortState,
} from "./material-catalogue";

function mat(over: Partial<QuotingMaterial>): QuotingMaterial {
  return {
    grade: "X",
    display_name: "X",
    density_g_cm3: 1,
    cost_per_kg_eur: 1,
    machinability_index: 1,
    carbide_life_multiplier: 1,
    stock_status: "in_stock",
    lead_time_default_days: 0,
    quote_multiplier: 1,
    notes: null,
    updated_at: "2026-01-01T00:00:00Z",
    updated_by_actor: "ervin",
    ...over,
  };
}

describe("stock status vocab", () => {
  test("every ordered status has a label and a tone (no orphan)", () => {
    for (const s of STOCK_STATUS_ORDER) {
      expect(stockStatusLabel(s)).toBeTruthy();
      expect(stockStatusTone(s)).toBeTruthy();
    }
    // order is the 4-value sourcing vocab from the brief
    expect(STOCK_STATUS_ORDER).toEqual([
      "in_stock",
      "source_1_2d",
      "source_3_7d",
      "special_order",
    ]);
  });

  test("unknown status degrades to raw value + warns", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    expect(stockStatusLabel("on_order")).toBe("on_order");
    expect(warn).toHaveBeenCalledOnce();
    warn.mockRestore();
  });
});

describe("toggleSort three-click cycle", () => {
  test("unsorted → asc → desc → unsorted", () => {
    let s: SortState = { key: null, dir: "asc" };
    s = toggleSort(s, "cost_per_kg_eur");
    expect(s).toEqual({ key: "cost_per_kg_eur", dir: "asc" });
    s = toggleSort(s, "cost_per_kg_eur");
    expect(s).toEqual({ key: "cost_per_kg_eur", dir: "desc" });
    s = toggleSort(s, "cost_per_kg_eur");
    expect(s).toEqual({ key: null, dir: "asc" });
  });

  test("switching column resets to asc", () => {
    const s = toggleSort({ key: "grade", dir: "desc" }, "density_g_cm3");
    expect(s).toEqual({ key: "density_g_cm3", dir: "asc" });
  });
});

describe("compareMaterials / sortMaterials", () => {
  test("numeric sort ascending and descending", () => {
    const rows = [
      mat({ grade: "A", cost_per_kg_eur: 50 }),
      mat({ grade: "B", cost_per_kg_eur: 6 }),
      mat({ grade: "C", cost_per_kg_eur: 35 }),
    ];
    const asc = sortMaterials(rows, { key: "cost_per_kg_eur", dir: "asc" });
    expect(asc.map((m) => m.grade)).toEqual(["B", "C", "A"]);
    const desc = sortMaterials(rows, { key: "cost_per_kg_eur", dir: "desc" });
    expect(desc.map((m) => m.grade)).toEqual(["A", "C", "B"]);
  });

  test("stock_status sorts by sourcing tier, not alphabetically", () => {
    const rows = [
      mat({ grade: "A", stock_status: "special_order" }),
      mat({ grade: "B", stock_status: "in_stock" }),
      mat({ grade: "C", stock_status: "source_3_7d" }),
    ];
    const asc = sortMaterials(rows, { key: "stock_status", dir: "asc" });
    expect(asc.map((m) => m.stock_status)).toEqual([
      "in_stock",
      "source_3_7d",
      "special_order",
    ]);
  });

  test("ties break on grade ascending (stable)", () => {
    const rows = [
      mat({ grade: "Zeta", cost_per_kg_eur: 10 }),
      mat({ grade: "Alpha", cost_per_kg_eur: 10 }),
    ];
    const asc = sortMaterials(rows, { key: "cost_per_kg_eur", dir: "asc" });
    expect(asc.map((m) => m.grade)).toEqual(["Alpha", "Zeta"]);
    // even descending, the tiebreak stays grade-ascending
    const desc = sortMaterials(rows, { key: "cost_per_kg_eur", dir: "desc" });
    expect(desc.map((m) => m.grade)).toEqual(["Alpha", "Zeta"]);
  });

  test("no sort key returns input order untouched (and does not mutate)", () => {
    const rows = [mat({ grade: "B" }), mat({ grade: "A" })];
    const out = sortMaterials(rows, { key: null, dir: "asc" });
    expect(out.map((m) => m.grade)).toEqual(["B", "A"]);
    expect(rows.map((m) => m.grade)).toEqual(["B", "A"]);
  });

  test("compareMaterials direct: display_name localeCompare", () => {
    expect(
      compareMaterials(
        mat({ display_name: "Aluminium" }),
        mat({ display_name: "Steel" }),
        "display_name",
        "asc",
      ),
    ).toBeLessThan(0);
  });
});

describe("renderPushStatusSuffix (S339)", () => {
  const NOW = Date.parse("2026-06-10T12:00:00.000Z");

  function status(over: Partial<CataloguePushStatus>): CataloguePushStatus {
    return {
      running: true,
      paused: false,
      last_attempt_at: null,
      last_outcome: null,
      last_pushed_count: null,
      last_detail: null,
      ...over,
    };
  }

  test("fresh success → positive ✓", () => {
    const r = renderPushStatusSuffix(
      status({
        last_outcome: "ok",
        last_attempt_at: "2026-06-10T11:45:00.000Z", // 15 min ago
        last_pushed_count: 12,
      }),
      NOW,
    );
    expect(r.tone).toBe("positive");
    expect(r.text).toContain("✓");
  });

  test("stale success (>30 min) → muted pending, not a green tick", () => {
    const r = renderPushStatusSuffix(
      status({
        last_outcome: "ok",
        last_attempt_at: "2026-06-10T11:00:00.000Z", // 60 min ago
      }),
      NOW,
    );
    expect(r.tone).toBe("muted");
    expect(r.text).not.toContain("✓");
  });

  test("unexpected_status (the prod 403/400 shape) → warning failing", () => {
    const r = renderPushStatusSuffix(
      status({
        last_outcome: "unexpected_status",
        last_attempt_at: "2026-06-10T11:59:00.000Z",
        last_detail: "HTTP 403",
      }),
      NOW,
    );
    expect(r.tone).toBe("warning");
    expect(r.text).toContain("⚠");
    expect(r.text.toLowerCase()).toContain("failing");
  });

  test("transport error → warning failing", () => {
    const r = renderPushStatusSuffix(
      status({ last_outcome: "transport", last_attempt_at: NOW.toString() }),
      NOW,
    );
    expect(r.tone).toBe("warning");
  });

  test("paused on 401 → warning re-paste prompt, overrides outcome", () => {
    const r = renderPushStatusSuffix(
      status({ paused: true, last_outcome: "unauthorized" }),
      NOW,
    );
    expect(r.tone).toBe("warning");
    expect(r.text.toLowerCase()).toContain("re-paste");
  });

  test("dormant → muted, non-alarming", () => {
    const r = renderPushStatusSuffix(status({ last_outcome: "dormant" }), NOW);
    expect(r.tone).toBe("muted");
    expect(r.text.toLowerCase()).toContain("not configured");
  });

  test("never attempted (null outcome) → muted pending", () => {
    const r = renderPushStatusSuffix(status({ last_outcome: null }), NOW);
    expect(r.tone).toBe("muted");
    expect(r.text.toLowerCase()).toContain("pending");
  });
});
