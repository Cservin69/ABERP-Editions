// S349 / PR-40 (U1) — vitest pins for the Auto-Quoting detail-panel
// helpers. The panel's three derived sections (status timeline, last
// writeback outcome, pricing breakdown table) are pure functions over
// the fetched data, extracted here so they're testable without
// component-render tooling (same posture as `pricing-failure-kind.ts`).

import { describe, it, expect } from "vitest";

import type { AuditEntryView } from "./api";
import type { PricingBreakdownView } from "./api";
import {
  auditKindLabel,
  breakdownRows,
  latestWritebackOutcome,
  reasoningLogLines,
  timelineNodes,
  WRITEBACK_OUTCOME_KIND,
} from "./pricing-job-detail";
import { writebackOutcomeBadge } from "./pricing-failure-kind";

/** Build a mock audit event. `kind` is the Rust Debug-variant string
 *  the backend serialises (e.g. `QuotePricingPosted`). */
function ev(
  seq: number,
  kind: string,
  occurred_at: string,
  actor = "system",
  payload: Record<string, unknown> = {},
): AuditEntryView {
  return {
    seq,
    kind,
    actor,
    occurred_at,
    chain_base_invoice_id: null,
    payload,
  };
}

// A realistic newest-first audit page for one priced row.
const MOCK_PAGE: AuditEntryView[] = [
  ev(5, WRITEBACK_OUTCOME_KIND, "2026-06-11T10:05:00Z", "system", {
    outcome: "routing_misconfigured",
    http_status: 200,
    content_type: "text/html",
    body_excerpt: "<!doctype html>",
    retryable: false,
    attempt_n: 1,
  }),
  ev(4, "QuotePricingFailed", "2026-06-11T10:04:00Z"),
  ev(3, "QuotePricingRendered", "2026-06-11T10:03:00Z"),
  ev(2, "QuotePricingPriced", "2026-06-11T10:02:00Z"),
  ev(1, "QuotePricingFetched", "2026-06-11T10:01:00Z"),
];

describe("timelineNodes", () => {
  it("keeps only lifecycle transitions and orders them oldest-first", () => {
    const nodes = timelineNodes(MOCK_PAGE);
    // Fetched, Priced, Rendered, Failed are lifecycle; the writeback-
    // outcome diagnostic row is NOT a timeline node.
    expect(nodes.map((n) => n.kind)).toEqual([
      "QuotePricingFetched",
      "QuotePricingPriced",
      "QuotePricingRendered",
      "QuotePricingFailed",
    ]);
    // Chronological: first node is the oldest event.
    expect(nodes[0].occurred_at).toBe("2026-06-11T10:01:00Z");
    expect(nodes[0].label).toContain("Fetched");
  });

  it("surfaces the actor (auto vs operator) on each node", () => {
    const page = [ev(1, "QuotePricingFetched", "t", "operator-bob")];
    expect(timelineNodes(page)[0].actor).toBe("operator-bob");
  });

  it("returns an empty list for a page with no lifecycle events", () => {
    expect(timelineNodes([])).toEqual([]);
  });
});

describe("latestWritebackOutcome", () => {
  it("lifts the newest writeback outcome's structured fields", () => {
    const wb = latestWritebackOutcome(MOCK_PAGE);
    expect(wb).not.toBeNull();
    expect(wb!.outcome).toBe("routing_misconfigured");
    expect(wb!.http_status).toBe(200);
    expect(wb!.content_type).toBe("text/html");
    expect(wb!.body_excerpt).toBe("<!doctype html>");
    expect(wb!.retryable).toBe(false);
    expect(wb!.attempt_n).toBe(1);
    expect(wb!.occurred_at).toBe("2026-06-11T10:05:00Z");
  });

  it("returns null when no writeback has been attempted", () => {
    const page = [ev(1, "QuotePricingFetched", "t")];
    expect(latestWritebackOutcome(page)).toBeNull();
  });

  it("tolerates a partial payload (transport failure, no http_status)", () => {
    const page = [
      ev(1, WRITEBACK_OUTCOME_KIND, "t", "system", {
        outcome: "timeout",
        retryable: true,
      }),
    ];
    const wb = latestWritebackOutcome(page);
    expect(wb!.outcome).toBe("timeout");
    expect(wb!.http_status).toBeNull();
    expect(wb!.content_type).toBeNull();
    expect(wb!.retryable).toBe(true);
  });
});

describe("writebackOutcomeBadge", () => {
  it("maps success to a GREEN badge", () => {
    expect(writebackOutcomeBadge("success").className).toBe("chip chip--ok");
  });

  it("maps routing_misconfigured to the RED routing badge", () => {
    const b = writebackOutcomeBadge("routing_misconfigured");
    expect(b.className).toBe("chip chip--err");
    // S368 — the chip now names the 404-masked-by-CloudFront cause too.
    expect(b.label).toContain("masked by CloudFront");
    expect(b.label).toContain("404");
  });

  it("surfaces an unknown tag verbatim rather than dropping it", () => {
    const b = writebackOutcomeBadge("some_future_tag");
    expect(b.label).toBe("some_future_tag");
  });
});

describe("breakdownRows", () => {
  it("emits the monetary lines present, in order", () => {
    const bd: PricingBreakdownView = {
      material_cost: 10,
      labor_cost: 20,
      setup_cost: 5,
      overhead: 3,
      margin: 7,
      total_price: 45,
      machining_minutes: 12,
    };
    const rows = breakdownRows(bd);
    expect(rows.map((r) => r.value)).toEqual([10, 20, 5, 3, 7, 45]);
    expect(rows[0].label).toContain("Material");
    expect(rows[5].label).toContain("Total");
  });

  it("omits a line that is absent rather than rendering 0.00", () => {
    const bd: PricingBreakdownView = { total_price: 45 };
    const rows = breakdownRows(bd);
    expect(rows).toHaveLength(1);
    expect(rows[0].value).toBe(45);
  });

  it("ADVERSARIAL: returns [] for a null breakdown (graceful, no crash)", () => {
    // Mirrors the backend adversarial case: a row that never reached
    // Pricing has breakdown === null. The panel renders a "not
    // available" placeholder when this is empty — never a blank table.
    expect(breakdownRows(null)).toEqual([]);
  });
});

describe("reasoningLogLines", () => {
  // S404 — the operator (and the customer PDF) must see EVERY reasoning
  // line, never a top-N cap. These pins fire if anyone reintroduces a
  // `.slice(...)` on the log (the exact regression the old PDF had).
  for (const n of [3, 12, 50, 100]) {
    it(`returns all ${n} lines, uncapped and in order`, () => {
      const log = Array.from({ length: n }, (_, i) => `step ${i}`);
      const bd: PricingBreakdownView = { total_price: 1, reasoning_log: log };
      const out = reasoningLogLines(bd);
      expect(out).toHaveLength(n);
      expect(out).toEqual(log);
      expect(out[0]).toBe("step 0");
      expect(out[n - 1]).toBe(`step ${n - 1}`);
    });
  }

  it("returns [] for a null breakdown (graceful, no crash)", () => {
    expect(reasoningLogLines(null)).toEqual([]);
  });

  it("returns [] when the breakdown has no reasoning_log field", () => {
    expect(reasoningLogLines({ total_price: 45 })).toEqual([]);
  });
});

describe("auditKindLabel", () => {
  it("gives bilingual labels for the pipeline lifecycle", () => {
    expect(auditKindLabel("QuotePricingPosted")).toContain("Posted");
    expect(auditKindLabel("QuotePricingPosted")).toContain("Visszaküldve");
  });

  it("surfaces an unknown kind verbatim (CLAUDE.md #12)", () => {
    expect(auditKindLabel("SomeFutureKind")).toBe("SomeFutureKind");
  });
});
