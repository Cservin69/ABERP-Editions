import { describe, expect, it } from "vitest";
// Vite's `?raw` — the component sources as strings. Same posture as
// `statistics-integrity-banner.test.ts`: this package mounts no
// components, so the contract is pinned by reading the source. Honest
// scope — these cannot prove the lists RENDER the right rows; they catch
// the one regression with a plausible motive, named per-test below.
import outgoing from "../routes/InvoiceList.svelte?raw";
import incoming from "../routes/IncomingInvoiceList.svelte?raw";

// ─────────────────────────────────────────────────────────────────────
// The dashboard's aging panels age an outstanding invoice with a missing
// or unreadable `payment_deadline` into `d90_plus` instead of dropping it
// (`reports::aging_placement`), which is what makes the buckets sum to
// the receivables / payables total. Each aging tile is CLICKABLE: it deep
// links into one of these two lists via `?aging=`, and the list re-runs
// the classification client-side.
//
// So the tile and its list must agree on undated rows or the fix trades
// one incoherence for another — the operator clicks "90+ nap = 3" and
// lands on a list showing 2. Both consumers previously short-circuited on
// `payment_deadline === null`; `aging.ts` moving to a total function does
// NOT stop a caller from re-adding that early-out, and a full revert of
// both stayed green before these pins existed.
//
// The hygiene facet is the deliberate exception and is pinned separately
// below — see `payable_past_deadline_count`.
// ─────────────────────────────────────────────────────────────────────

/** Any shape of "bail out because the deadline is absent". Deliberately
 * broader than the literal `=== null` the two components used, so the
 * pin is not sidestepped by `== null`, a falsy check, or an early
 * `!deadline` guard. */
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

  it("outgoing list does NOT exclude rows with a null deadline", () => {
    // The revert this pin exists for: re-adding
    // `if (row.payment_deadline === null) return false;` makes the
    // receivables 90+ tile over-count its own drill-down, silently.
    expect(outgoingAging).not.toMatch(NULL_DEADLINE_EXCLUSION);
  });

  it("incoming list classifies through the shared helper, not its own copy", () => {
    expect(incomingAging).toContain("agingBucketFor(");
  });

  it("incoming list does NOT exclude rows with a null deadline", () => {
    // Load-bearing on this side: `ap_sync` records no deadline at all for
    // NAV-synced payables, so excluding them would empty the payables
    // aging drill-down against a fully-populated 90+ tile.
    expect(incomingAging).not.toMatch(NULL_DEADLINE_EXCLUSION);
  });
});

describe("the past-deadline HYGIENE facet keeps excluding undated rows", () => {
  // The counterpart contract, and the reason this is not simply "never
  // exclude null deadlines anywhere". An aging bucket is an age ESTIMATE,
  // so imputing 90+ is defensible and disclosed.
  // `payable_past_deadline_count` is a LATENESS ASSERTION, and nothing
  // supports one for an invoice with no deadline — so the backend counter
  // skips undated rows (`reports::aggregate_ap`) and this list, which is
  // that tile's drill-down, must skip them too. Dropping this early-out
  // to "match" the aging facet would make the list over-count the tile
  // instead.
  it("still short-circuits on a null deadline", () => {
    expect(incomingHygiene).toMatch(NULL_DEADLINE_EXCLUSION);
  });

  it("still requires a deadline strictly in the past", () => {
    expect(incomingHygiene).toContain("todayIso()");
  });

  it("is a DIFFERENT predicate from the aging facet on exactly this point", () => {
    // If both blocks ever agree about null deadlines, one of the two
    // tiles has stopped matching its own drill-down.
    expect(NULL_DEADLINE_EXCLUSION.test(incomingHygiene)).toBe(true);
    expect(NULL_DEADLINE_EXCLUSION.test(incomingAging)).toBe(false);
  });
});
