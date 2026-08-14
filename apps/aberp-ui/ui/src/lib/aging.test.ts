// S262 / PR-251 — pins the aging-bucket boundaries against the backend
// `reports::aging_bucket_for`. If these drift, a dashboard bucket count
// and its click-through-filtered list disagree.

import { describe, it, expect } from "vitest";
import {
  agingBucketFor,
  parseAgingBucket,
  panelField,
  AGING_BUCKETS,
  type AgingBucket,
} from "./aging";

const TODAY = "2026-06-30";

describe("agingBucketFor — boundaries mirror reports::aging_bucket_for", () => {
  // overdue_days = today − deadline; thresholds at 0 / 30 / 60 / 90.
  const cases: Array<[string, AgingBucket]> = [
    ["2026-07-15", "current"], // future deadline → not due
    ["2026-06-30", "current"], // due today → overdue 0 → current
    ["2026-06-29", "d1_30"], // overdue 1
    ["2026-05-31", "d1_30"], // overdue 30 (boundary, inclusive)
    ["2026-05-30", "d31_60"], // overdue 31
    ["2026-05-01", "d31_60"], // overdue 60 (boundary)
    ["2026-04-30", "d61_90"], // overdue 61
    ["2026-04-01", "d61_90"], // overdue 90 (boundary)
    ["2026-03-31", "d90_plus"], // overdue 91
  ];
  for (const [deadline, bucket] of cases) {
    it(`${deadline} → ${bucket}`, () => {
      expect(agingBucketFor(TODAY, deadline)).toBe(bucket);
    });
  }

  // The backend used to drop an outstanding invoice with a missing or
  // unreadable `payment_deadline` out of every aging bucket while still
  // counting it in the receivables/payables total, so the panel's
  // breakdown summed to less than its own headline. It now ages such a
  // row as `d90_plus` (`reports::aging_placement`). This mirror MUST move
  // with it: if it kept excluding those rows, the operator would click
  // "90+ nap = 3" and land on a list showing 2 — the exact drift this
  // shared module exists to prevent.
  it("ages an unreadable deadline as d90_plus, never excluded", () => {
    expect(agingBucketFor(TODAY, "not-a-date")).toBe("d90_plus");
    expect(agingBucketFor(TODAY, "30/06/2026")).toBe("d90_plus");
  });

  it("ages a MISSING deadline as d90_plus, never excluded", () => {
    expect(agingBucketFor(TODAY, null)).toBe("d90_plus");
    expect(agingBucketFor(TODAY, undefined)).toBe("d90_plus");
  });

  it("never returns a non-bucket, so no caller can silently drop a row", () => {
    // The old signature returned `AgingBucket | null` and every caller
    // read the null as "exclude". Restoring that return type is the
    // mutation this pin is aimed at.
    for (const deadline of ["2026-05-31", "not-a-date", null, undefined]) {
      expect(AGING_BUCKETS).toContain(agingBucketFor(TODAY, deadline));
    }
  });
});

describe("parseAgingBucket — closed vocab", () => {
  it("accepts every legal bucket", () => {
    for (const b of AGING_BUCKETS) expect(parseAgingBucket(b)).toBe(b);
  });
  it("discards unknown vocab", () => {
    expect(parseAgingBucket("days_1_30")).toBeNull();
    expect(parseAgingBucket("")).toBeNull();
    expect(parseAgingBucket("CURRENT")).toBeNull();
  });
});

describe("panelField — maps to the AgingPanel wire keys", () => {
  it("maps each bucket to its backend field name", () => {
    expect(panelField("current")).toBe("current");
    expect(panelField("d1_30")).toBe("days_1_30");
    expect(panelField("d31_60")).toBe("days_31_60");
    expect(panelField("d61_90")).toBe("days_61_90");
    expect(panelField("d90_plus")).toBe("days_90_plus");
  });
});
