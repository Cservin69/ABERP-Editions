import { describe, expect, it } from "vitest";
import { AGING_BUCKETS, type AgingBucket } from "./aging";
import {
  incomingAgingMatches,
  incomingPastDeadlineMatches,
  outgoingAgingMatches,
  type IncomingAgingRow,
  type OutgoingAgingRow,
} from "./aging-facets";

// ─────────────────────────────────────────────────────────────────────
// BEHAVIOUR pins for the three drill-down predicates.
//
// These execute the filters against rows. The previous version of this
// suite asserted on component SOURCE TEXT via `?raw` + regex, which was
// vacuous in the one direction that matters: flipping a verdict
// (`return false` → `return true` at the exclusion) leaves the grepped
// text in place and the suite stays green while the list starts showing
// rows the dashboard calls settled. Every pin below fails under that
// mutation.
//
// The contract they hold: under `reports::aging_placement` a row with no
// READABLE `payment_deadline` is off the books entirely — out of the
// outstanding total, out of every aging bucket, out of the past-deadline
// counters. Both lists must agree, or a tile and its drill-down disagree
// about whether money is owed.
// ─────────────────────────────────────────────────────────────────────

const TODAY = "2026-08-13";

function arRow(overrides: Partial<OutgoingAgingRow> = {}): OutgoingAgingRow {
  return {
    payment: null,
    is_storno: false,
    state: "Submitted",
    payment_deadline: "2026-07-14", // 30 days overdue → d1_30
    ...overrides,
  };
}

function apRow(overrides: Partial<IncomingAgingRow> = {}): IncomingAgingRow {
  return {
    local_status: "Outstanding",
    payment_deadline: "2026-07-14",
    ...overrides,
  };
}

/** Every shape of "no readable deadline" the backend classifies settled. */
const UNREADABLE_DEADLINES = [null, "", "not-a-date", "2026-02-30", "2026-06-15T00:00:00Z", "15/06/2026"];

describe("AR aging drill-down — undated rows match NO bucket", () => {
  for (const deadline of UNREADABLE_DEADLINES) {
    it(`${JSON.stringify(deadline)} matches none of the five buckets`, () => {
      const row = arRow({ payment_deadline: deadline });
      for (const bucket of AGING_BUCKETS) {
        expect(
          outgoingAgingMatches(row, bucket, TODAY),
          `an unreadable deadline must not appear in the ${bucket} drill-down — the tile ` +
            `excludes it from the receivables total entirely`,
        ).toBe(false);
      }
    });
  }

  it("a readable deadline still lands in exactly ONE bucket", () => {
    // The exclusion must not have swallowed the healthy path: a genuine
    // receivable still has to be reachable from its tile.
    const row = arRow({ payment_deadline: "2026-07-14" });
    const matched = AGING_BUCKETS.filter((b) => outgoingAgingMatches(row, b, TODAY));
    expect(matched).toEqual(["d1_30"]);
  });

  it("boundary rows are placed, not dropped", () => {
    const cases: Array<[string, AgingBucket]> = [
      ["2026-08-23", "current"], // future
      ["2026-08-13", "current"], // due today
      ["2026-08-12", "d1_30"], // 1 day over
      ["2026-06-04", "d61_90"],
      ["2026-01-01", "d90_plus"],
    ];
    for (const [deadline, expected] of cases) {
      const matched = AGING_BUCKETS.filter((b) =>
        outgoingAgingMatches(arRow({ payment_deadline: deadline }), b, TODAY),
      );
      expect(matched, `${deadline} should be ${expected}`).toEqual([expected]);
    }
  });

  it("keeps its non-deadline exclusions (paid / storno / not counted)", () => {
    // Guards the extraction itself: moving the predicate out of the
    // component must not have dropped a condition on the way.
    expect(outgoingAgingMatches(arRow({ payment: {} }), "d1_30", TODAY)).toBe(false);
    expect(outgoingAgingMatches(arRow({ is_storno: true }), "d1_30", TODAY)).toBe(false);
    expect(outgoingAgingMatches(arRow({ state: "Draft" }), "d1_30", TODAY)).toBe(false);
    for (const state of ["Submitted", "Recovered", "Finalized"]) {
      expect(outgoingAgingMatches(arRow({ state }), "d1_30", TODAY)).toBe(true);
    }
  });

  it("passes everything through when no aging facet is active", () => {
    expect(outgoingAgingMatches(arRow({ payment_deadline: null }), null, TODAY)).toBe(true);
  });
});

describe("AP aging drill-down — undated rows match NO bucket", () => {
  // Load-bearing side: `ap_sync` records no deadline on NAV-synced
  // payables, so this is most of the AP book. A verdict flip here fills
  // a drill-down whose tile reads zero.
  for (const deadline of UNREADABLE_DEADLINES) {
    it(`${JSON.stringify(deadline)} matches none of the five buckets`, () => {
      const inv = apRow({ payment_deadline: deadline });
      for (const bucket of AGING_BUCKETS) {
        expect(incomingAgingMatches(inv, bucket, TODAY)).toBe(false);
      }
    });
  }

  it("a readable deadline still lands in exactly ONE bucket", () => {
    const matched = AGING_BUCKETS.filter((b) => incomingAgingMatches(apRow(), b, TODAY));
    expect(matched).toEqual(["d1_30"]);
  });

  it("keeps the Outstanding-only gate", () => {
    expect(incomingAgingMatches(apRow({ local_status: "Settled" }), "d1_30", TODAY)).toBe(false);
    expect(incomingAgingMatches(apRow({ local_status: "Irrelevant" }), "d1_30", TODAY)).toBe(false);
  });

  it("passes everything through when no aging facet is active", () => {
    expect(incomingAgingMatches(apRow({ payment_deadline: null }), null, TODAY)).toBe(true);
  });
});

describe("AP past-deadline hygiene drill-down", () => {
  it("excludes every unreadable deadline", () => {
    for (const deadline of UNREADABLE_DEADLINES) {
      expect(
        incomingPastDeadlineMatches(apRow({ payment_deadline: deadline }), TODAY),
        `${JSON.stringify(deadline)} is settled, not late`,
      ).toBe(false);
    }
  });

  it("does NOT lexicographically order an unreadable deadline", () => {
    // The specific bug this predicate carried before extraction: it
    // compared `payment_deadline >= today` as raw strings after only a
    // `!== null` check. "2026-02-30" sorts before "2026-08-13", so it
    // counted as past-deadline while the backend classified it settled
    // and left it out of `payable_past_deadline_count`.
    expect("2026-02-30" < TODAY).toBe(true); // the lexicographic trap
    expect(incomingPastDeadlineMatches(apRow({ payment_deadline: "2026-02-30" }), TODAY)).toBe(
      false,
    );
  });

  it("zero-pads the YEAR before comparing", () => {
    // Unpadded, `${parsed.y}` renders year 999 as "999-01-01", which
    // sorts AFTER "2026-08-13" — so an ancient deadline would read as
    // NOT late. Three- and one-digit years are absurd as data, but the
    // comparison is lexicographic and a wrong verdict here is silent.
    expect("999-01-01" > TODAY).toBe(true); // the unpadded trap
    expect(incomingPastDeadlineMatches(apRow({ payment_deadline: "0999-01-01" }), TODAY)).toBe(
      true,
    );
    expect(incomingPastDeadlineMatches(apRow({ payment_deadline: "0001-01-01" }), TODAY)).toBe(
      true,
    );
  });

  it("still counts a genuinely late, readable deadline", () => {
    expect(incomingPastDeadlineMatches(apRow({ payment_deadline: "2026-08-12" }), TODAY)).toBe(true);
    expect(incomingPastDeadlineMatches(apRow({ payment_deadline: "  2026-08-12  " }), TODAY)).toBe(
      true,
    );
  });

  it("excludes due-today and future deadlines", () => {
    expect(incomingPastDeadlineMatches(apRow({ payment_deadline: TODAY }), TODAY)).toBe(false);
    expect(incomingPastDeadlineMatches(apRow({ payment_deadline: "2026-08-14" }), TODAY)).toBe(
      false,
    );
  });

  it("keeps the Outstanding-only gate", () => {
    expect(
      incomingPastDeadlineMatches(
        apRow({ local_status: "Settled", payment_deadline: "2026-08-12" }),
        TODAY,
      ),
    ).toBe(false);
  });
});

describe("the aging and hygiene facets agree about undated rows", () => {
  it("both exclude a deadline-less payable", () => {
    // Same conclusion, two reasons: it is not outstanding (aging), and a
    // settled invoice is not a late one (hygiene). If these ever split,
    // one tile has stopped matching its own drill-down.
    const inv = apRow({ payment_deadline: null });
    for (const bucket of AGING_BUCKETS) {
      expect(incomingAgingMatches(inv, bucket, TODAY)).toBe(false);
    }
    expect(incomingPastDeadlineMatches(inv, TODAY)).toBe(false);
  });
});
