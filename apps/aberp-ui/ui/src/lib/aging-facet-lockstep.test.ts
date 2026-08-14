import { describe, expect, it } from "vitest";
// Vite's `?raw` — the component sources as strings. Same posture as
// `statistics-integrity-banner.test.ts`: this package mounts no
// components, so the contract is pinned by reading the source. Honest
// scope — these cannot prove the lists RENDER the right rows; they catch
// the one regression with a plausible motive, named per-test below.
import outgoing from "../routes/InvoiceList.svelte?raw";
import incoming from "../routes/IncomingInvoiceList.svelte?raw";

// ─────────────────────────────────────────────────────────────────────
// An outstanding invoice with a missing or unreadable `payment_deadline`
// is treated as a SETTLED legacy import: the backend excludes it from the
// outstanding total, from every aging bucket, and from the past-deadline
// counters (`reports::aging_placement`). That single exclusion is what
// makes `sum(buckets) == total` hold.
//
// Each aging tile is CLICKABLE: it deep links into one of these two lists
// via `?aging=`, and the list re-runs the classification client-side. So
// both lists must exclude deadline-less rows too, or the operator clicks
// "90+ nap = 2" and lands on a list showing 5 — rows the dashboard has
// already declared settled.
//
// The hygiene facet excludes them as well, for its own reason (a settled
// invoice is not a late invoice). Under this behaviour the two facets
// AGREE about null deadlines — that agreement is itself pinned below, so
// a future change cannot quietly split them again.
// ─────────────────────────────────────────────────────────────────────

/** Any shape of "bail out because the deadline is absent". Deliberately
 * broader than the literal `=== null` the components use, so the pin
 * still passes if someone rewrites it as `== null` or a falsy check —
 * and still FAILS if the guard is deleted outright. */
const NULL_DEADLINE_EXCLUSION = /payment_deadline\s*(===?|!==?)\s*null|!\w+\.payment_deadline/;

/** Slice `source` from `startMarker` to the first line that closes at
 * `indent` spaces, so a block's own nested closers do not end it. */
function block(source: string, startMarker: string, indent: number): string {
  const start = source.indexOf(startMarker);
  expect(start, `expected to find \`${startMarker}\``).toBeGreaterThan(-1);
  const closer = `\n${" ".repeat(indent)}}`;
  const end = source.indexOf(closer, start);
  expect(end, `expected \`${startMarker}\` to close`).toBeGreaterThan(start);
  return source.slice(start, end);
}

const outgoingAging = block(outgoing, "function agingMatches(row: InvoiceListItem): boolean {", 2);
const incomingAging = block(incoming, "if (agingFacet !== null) {", 4);
const incomingHygiene = block(incoming, 'if (hygiene === "past_deadline") {', 4);

describe("aging click-through stays in lockstep with the dashboard panels", () => {
  it("outgoing list classifies through the shared helper, not its own copy", () => {
    // A local re-implementation of the bucket boundaries is the other way
    // these drift; `aging.ts` exists to be the single source.
    expect(outgoingAging).toContain("agingBucketFor(");
  });

  it("outgoing list EXCLUDES rows with a null deadline", () => {
    // The revert this pin exists for: dropping the early-out puts rows
    // the dashboard counts as settled back into the receivables aging
    // drill-down, so the 90+ tile under-counts its own list.
    expect(outgoingAging).toMatch(NULL_DEADLINE_EXCLUSION);
  });

  it("incoming list classifies through the shared helper, not its own copy", () => {
    expect(incomingAging).toContain("agingBucketFor(");
  });

  it("incoming list EXCLUDES rows with a null deadline", () => {
    // Load-bearing on this side: `ap_sync` records no deadline at all for
    // NAV-synced payables, so without the early-out essentially the whole
    // AP book would appear in a drill-down whose tile reads zero.
    expect(incomingAging).toMatch(NULL_DEADLINE_EXCLUSION);
  });
});

describe("the past-deadline HYGIENE facet also excludes undated rows", () => {
  // Same conclusion, independent reason: a settled invoice is not a late
  // invoice, and `payable_past_deadline_count` is a lateness assertion.
  it("still short-circuits on a null deadline", () => {
    expect(incomingHygiene).toMatch(NULL_DEADLINE_EXCLUSION);
  });

  it("still requires a deadline strictly in the past", () => {
    expect(incomingHygiene).toContain("todayIso()");
  });

  it("AGREES with the aging facet about null deadlines", () => {
    // Both tiles now derive from the same "deadline-less means settled"
    // rule, so both drill-downs must exclude those rows. If these two
    // ever disagree, one tile has stopped matching its own list —
    // whichever direction the split happens in.
    expect(NULL_DEADLINE_EXCLUSION.test(incomingHygiene)).toBe(true);
    expect(NULL_DEADLINE_EXCLUSION.test(incomingAging)).toBe(true);
    expect(NULL_DEADLINE_EXCLUSION.test(outgoingAging)).toBe(true);
  });
});
