// S257 / PR-246 — pure-helper tests for the Adapters page formatting.
// Mirrors the draft-delete.test.ts posture: exercise the closed-vocab
// mappings without mounting a component (CLAUDE.md rule 9).

import { afterEach, describe, expect, it, vi } from "vitest";

import {
  ADAPTER_KIND_LABELS,
  ADAPTER_KIND_ORDER,
  adapterKindLabel,
  adapterStatusLabel,
  adapterStatusTone,
  isAlertingState,
  computeAdapterAlerts,
  initialAdapterAlertState,
} from "./adapter-format";
import type {
  AdapterKind,
  AdapterStatus,
  AdapterTransitionEntry,
} from "./api";

describe("adapterKindLabel", () => {
  it("labels every known kind", () => {
    expect(adapterKindLabel("barcode-scanner")).toBe("Barcode scanner");
    expect(adapterKindLabel("label-printer")).toBe("Zebra label printer");
    expect(adapterKindLabel("cnc-machine")).toBe("MTConnect CNC");
    expect(adapterKindLabel("robot")).toBe("UR RTDE robot");
    expect(adapterKindLabel("laser-cutter")).toBe("Trumpf laser cutter");
  });

  it("every ordered kind has a label (no orphan in the picker)", () => {
    for (const k of ADAPTER_KIND_ORDER) {
      expect(ADAPTER_KIND_LABELS[k]).toBeTruthy();
    }
    // The order set equals the label key set — no kind silently absent.
    expect(new Set(ADAPTER_KIND_ORDER).size).toBe(
      Object.keys(ADAPTER_KIND_LABELS).length,
    );
  });

  it("gracefully degrades on an unknown kind (renders raw + warns)", () => {
    // A SPA build older than the backend could receive a kind the
    // closed vocab here doesn't carry — it must NOT crash.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const out = adapterKindLabel("warp-drive" as AdapterKind);
    expect(out).toBe("warp-drive");
    expect(warn).toHaveBeenCalledOnce();
  });
});

describe("adapterStatusLabel + adapterStatusTone", () => {
  const cases: Array<[AdapterStatus, string, string]> = [
    ["healthy", "Healthy", "positive"],
    ["degraded", "Degraded", "warning"],
    ["unhealthy", "Unhealthy", "negative"],
    ["starting", "Starting", "warning"],
    ["stopped", "Stopped", "muted"],
  ];

  it.each(cases)("status %s → label %s, tone %s", (status, label, tone) => {
    expect(adapterStatusLabel(status)).toBe(label);
    expect(adapterStatusTone(status)).toBe(tone);
  });

  it("an unknown status falls back to the raw label + muted tone", () => {
    expect(adapterStatusLabel("quantum")).toBe("quantum");
    // muted, not a misleading positive/negative colour.
    expect(adapterStatusTone("quantum")).toBe("muted");
  });
});

// ── S258 / PR-247 — wall-TV alerting ────────────────────────────────

describe("isAlertingState", () => {
  it("degraded + unhealthy alarm; healthy/starting/stopped do not", () => {
    expect(isAlertingState("degraded")).toBe(true);
    expect(isAlertingState("unhealthy")).toBe(true);
    expect(isAlertingState("healthy")).toBe(false);
    expect(isAlertingState("starting")).toBe(false);
    expect(isAlertingState("stopped")).toBe(false);
    // Unknown is not alerting — no false alarm on an unrecognised status.
    expect(isAlertingState("quantum")).toBe(false);
  });
});

describe("computeAdapterAlerts", () => {
  function t(
    over: Partial<AdapterTransitionEntry> & { seq: number },
  ): AdapterTransitionEntry {
    return {
      adapter_id: "cnc-01",
      from_state: "healthy",
      to_state: "unhealthy",
      at_iso8601: "2026-06-05T10:00:00Z",
      ...over,
    };
  }

  it("first poll seeds the high-water-mark and chimes nothing (boot grace)", () => {
    const s0 = initialAdapterAlertState();
    const r = computeAdapterAlerts([t({ seq: 5 }), t({ seq: 7 })], s0);
    expect(r.chimeCount).toBe(0);
    // A tile already unhealthy on load must NOT fire — only an in-session
    // transition does. The seq is now acknowledged.
    expect(r.next.acknowledgedSeq).toBe(7);
  });

  it("chimes a fresh non-alerting→alerting transition after the seed", () => {
    const seeded = computeAdapterAlerts([t({ seq: 5 })], initialAdapterAlertState());
    const r = computeAdapterAlerts(
      [t({ seq: 5 }), t({ seq: 8, to_state: "degraded" })],
      seeded.next,
    );
    expect(r.chimeCount).toBe(1);
    expect(r.next.acknowledgedSeq).toBe(8);
  });

  it("does NOT chime a degraded→unhealthy escalation (already alerting)", () => {
    const seeded = computeAdapterAlerts([t({ seq: 5 })], initialAdapterAlertState());
    const r = computeAdapterAlerts(
      [t({ seq: 9, from_state: "degraded", to_state: "unhealthy" })],
      seeded.next,
    );
    expect(r.chimeCount).toBe(0);
  });

  it("coalesces multiple adapters degrading in one poll into one chime call (count = distinct)", () => {
    const seeded = computeAdapterAlerts([], initialAdapterAlertState());
    const r = computeAdapterAlerts(
      [
        t({ seq: 10, adapter_id: "cnc-01", to_state: "unhealthy" }),
        t({ seq: 11, adapter_id: "printer-A", to_state: "degraded" }),
      ],
      seeded.next,
    );
    expect(r.chimeCount).toBe(2);
  });

  it("debounces a flapping adapter — second alert within 30s is suppressed", () => {
    const seeded = computeAdapterAlerts([], initialAdapterAlertState());
    // First flap into unhealthy at 10:00:00.
    const a = computeAdapterAlerts(
      [t({ seq: 20, at_iso8601: "2026-06-05T10:00:00Z" })],
      seeded.next,
    );
    expect(a.chimeCount).toBe(1);
    // Recovered (no chime), then flaps into unhealthy again 12s later.
    const b = computeAdapterAlerts(
      [
        t({ seq: 21, from_state: "unhealthy", to_state: "healthy", at_iso8601: "2026-06-05T10:00:06Z" }),
        t({ seq: 22, at_iso8601: "2026-06-05T10:00:12Z" }),
      ],
      a.next,
    );
    expect(b.chimeCount).toBe(0);
    // A flap well outside the 30s window DOES chime again.
    const c = computeAdapterAlerts(
      [t({ seq: 23, at_iso8601: "2026-06-05T10:01:00Z" })],
      b.next,
    );
    expect(c.chimeCount).toBe(1);
  });

  it("a page reload reseeds from the ledger payload and replays nothing", () => {
    // Simulate: transition seq 8 chimed in the prior session.
    const seeded = computeAdapterAlerts([], initialAdapterAlertState());
    const fired = computeAdapterAlerts([t({ seq: 8 })], seeded.next);
    expect(fired.chimeCount).toBe(1);
    // Reload → fresh state; the same transition is still in the payload.
    const afterReload = computeAdapterAlerts([t({ seq: 8 })], initialAdapterAlertState());
    expect(afterReload.chimeCount).toBe(0);
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});
