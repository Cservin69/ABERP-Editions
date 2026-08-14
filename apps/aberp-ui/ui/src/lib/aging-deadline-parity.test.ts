import { describe, expect, it } from "vitest";
import { agingBucketFor, parseDeadline } from "./aging";

// ─────────────────────────────────────────────────────────────────────
// PARITY with the backend's `reports::parse_iso_date`.
//
// Whether a `payment_deadline` is readable is not a formatting question:
// it is what classifies an invoice SETTLED and removes it from the
// outstanding total, every aging bucket, and the past-deadline counters
// (`reports::aging_placement`). The SPA re-runs that same decision for
// the aging drill-downs. If the two disagree about one string, a tile
// and its list disagree about whether an invoice is on the books.
//
// This table is duplicated VERBATIM in
// `reports::tests::DEADLINE_PARITY_VOCAB`. Both sides must accept and
// reject identically — change one, change the other.
//
// The rows that matter most are the ones a naive parser gets WRONG
// rather than merely differently:
//   * `2026-02-30` — `Date.parse` silently rolls this to March 2 and
//     returns a valid number, so a naive SPA would bucket a row the
//     backend calls settled;
//   * `2026-06-15T00:00:00Z` — accepted by `Date.parse`, rejected by
//     `parse_iso_date`'s exact `[year]-[month]-[day]` format;
//   * `  2026-06-15  ` — the backend trims, so the SPA must too.
// ─────────────────────────────────────────────────────────────────────
const DEADLINE_PARITY_VOCAB: ReadonlyArray<readonly [string, boolean]> = [
  ["2026-06-15", true],
  ["  2026-06-15  ", true], // both sides trim
  ["2024-02-29", true], // real leap day
  ["2026-02-30", false], // day out of range for the month
  ["2025-02-29", false], // not a leap year
  ["2026-13-01", false], // month out of range
  ["2026-06-31", false], // June has 30 days
  ["2026-06-15T00:00:00Z", false], // trailing time component
  ["2026-06-15x", false], // trailing junk
  ["2026-6-5", false], // unpadded
  ["15/06/2026", false], // wrong order + separator
  ["", false],
  ["not-a-date", false],
];

const TODAY = "2026-06-30";

describe("parseDeadline mirrors reports::parse_iso_date exactly", () => {
  for (const [raw, expected] of DEADLINE_PARITY_VOCAB) {
    it(`${expected ? "accepts" : "rejects"} ${JSON.stringify(raw)}`, () => {
      expect(parseDeadline(raw) !== null).toBe(expected);
    });
  }

  it("rejects a missing deadline", () => {
    expect(parseDeadline(null)).toBeNull();
    expect(parseDeadline(undefined)).toBeNull();
  });

  it("does not roll over an impossible date the way bare Date.parse does", () => {
    // The specific regression: `Date.parse("2026-02-30T00:00:00Z")` is a
    // VALID number (March 2). A classifier built on it would put this row
    // in an aging bucket while the backend excluded it as settled — the
    // tile would read one number and its drill-down another.
    expect(Number.isNaN(Date.parse("2026-02-30T00:00:00Z"))).toBe(false);
    expect(parseDeadline("2026-02-30")).toBeNull();
  });
});

describe("the vocabulary drives bucketing, not just parsing", () => {
  // Parity at the parser is only useful if it reaches the decision the
  // drill-downs actually make. A row the backend calls settled must
  // classify to `null` here — no bucket at all.
  for (const [raw, expected] of DEADLINE_PARITY_VOCAB) {
    it(`${JSON.stringify(raw)} → ${expected ? "a bucket" : "null (settled)"}`, () => {
      const bucket = agingBucketFor(TODAY, raw);
      expect(bucket === null).toBe(!expected);
    });
  }
});
