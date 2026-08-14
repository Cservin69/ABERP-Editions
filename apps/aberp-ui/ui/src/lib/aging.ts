// S262 / PR-251 — pure aging-bucket helpers shared by the Finance
// dashboard (StatisticsPage) and the two invoice lists (InvoiceList /
// IncomingInvoiceList). No DOM, no fetch. Pinned by `aging.test.ts`.
//
// The bucket boundaries MIRROR the backend `reports::aging_bucket_for`
// (apps/aberp/src/reports.rs) EXACTLY so a dashboard bucket count and the
// click-through-filtered list agree on which rows fall in a bucket:
//
//     overdue_days = today − deadline   (whole calendar days)
//       <= 0  → current        (not yet due / due today)
//       1..30 → d1_30
//      31..60 → d31_60
//      61..90 → d61_90
//        > 90 → d90_plus
//     missing / unreadable deadline → null = NOT OUTSTANDING; the row is
//       a settled legacy import and is excluded from every bucket,
//       mirroring `reports::aging_placement`
//
// If the two ever drift, the operator clicks "31–60 nap = 3 invoices" and
// lands on a list showing 2 — the canonical fail-loud regression this
// shared module + its pins exist to prevent (CLAUDE.md rule 7/12).

import type { AgingPanel, AmountAggregate } from "./api";
import { parseIsoDate } from "./invoice-dates";

/** Closed-vocab aging bucket. The wire form (`d1_30` …) is the URL
 * deep-link token used by the dashboard → list click-through. */
export type AgingBucket = "current" | "d1_30" | "d31_60" | "d61_90" | "d90_plus";

/** Render / iteration order — overdue severity ascending. */
export const AGING_BUCKETS: readonly AgingBucket[] = [
  "current",
  "d1_30",
  "d31_60",
  "d61_90",
  "d90_plus",
];

/** Runtime membership table for URL-param validation (a hand-typed or
 * stale-bookmark `?aging=garbage` is discarded, never coerced). */
const LEGAL: ReadonlySet<string> = new Set(AGING_BUCKETS);

/** Parse an untrusted value into an [`AgingBucket`], or `null` if it is
 * absent or not in the closed vocab. Accepts `undefined` so callers can
 * pass a `Map.get` result directly. */
export function parseAgingBucket(s: string | null | undefined): AgingBucket | null {
  return s != null && LEGAL.has(s) ? (s as AgingBucket) : null;
}

/** Bilingual (HU primary, EN secondary) labels for the dashboard rows. */
export const AGING_LABELS: Readonly<Record<AgingBucket, string>> = {
  current: "Lejárat előtt / Not due",
  d1_30: "1–30 nap / days",
  d31_60: "31–60 nap / days",
  d61_90: "61–90 nap / days",
  d90_plus: "90+ nap / days",
};

/** Map a bucket to its field on the backend [`AgingPanel`] wire shape so
 * the dashboard reads `panel[fieldFor(bucket)]` in one expression. */
export function panelField(bucket: AgingBucket): keyof AgingPanel {
  switch (bucket) {
    case "current":
      return "current";
    case "d1_30":
      return "days_1_30";
    case "d31_60":
      return "days_31_60";
    case "d61_90":
      return "days_61_90";
    case "d90_plus":
      return "days_90_plus";
  }
}

/** Read a bucket's [`AmountAggregate`] off a panel. */
export function bucketAmount(panel: AgingPanel, bucket: AgingBucket): AmountAggregate {
  return panel[panelField(bucket)];
}

/** Decide whether a `payment_deadline` is READABLE, and parse it.
 *
 * This is the SPA half of a parity contract with the backend's
 * `reports::parse_iso_date`, and the contract is load-bearing: an
 * unreadable deadline is not a formatting nuisance, it is the signal that
 * classifies an invoice SETTLED and removes it from the outstanding
 * totals, every aging bucket, and the past-deadline counters
 * (`reports::aging_placement`). If the two sides disagree about one
 * string, a tile and its drill-down disagree about whether an invoice is
 * on the books.
 *
 * So the rules match `parse_iso_date` exactly: trim, then accept ONLY an
 * anchored `YYYY-MM-DD` — zero-padded, in range, no trailing content —
 * with a UTC round-trip that rejects dates JS would silently roll over
 * (`2026-02-30` becomes March 2 under a bare `Date.parse`). The shared
 * vocabulary is pinned on both sides; see
 * `aging-deadline-parity.test.ts` and
 * `reports::tests::DEADLINE_PARITY_VOCAB`.
 *
 * Delegates to `invoice-dates::parseIsoDate`, which is the canonical
 * strict parser. This module previously carried its own laxer
 * `Date.parse("…T00:00:00Z")` copy — that accepted `2026-02-30` and
 * timestamp-shaped strings the backend rejects, so the two classifiers
 * disagreed on exactly the inputs that matter most. */
export function parseDeadline(raw: string | null | undefined): IsoParts | null {
  if (raw == null) return null;
  return parseIsoDate(asciiTrim(raw));
}

/** ASCII whitespace only — NOT `String.prototype.trim`.
 *
 * JS `trim` strips the full Unicode whitespace set PLUS U+FEFF
 * (zero-width no-break space). Rust's `str::trim` strips the Unicode
 * `White_Space` property, which does NOT include U+FEFF. So a
 * BOM-prefixed deadline is trimmed and ACCEPTED by a naive SPA and
 * rejected by the backend — the two classifiers disagree, and under the
 * settled-exclusion rule they disagree about whether an invoice is on
 * the books.
 *
 * Rather than chase Rust's Unicode table from JS, both sides narrow to
 * the same small, explicit set: ASCII whitespace. `reports::parse_iso_date`
 * uses `trim_matches(char::is_ascii_whitespace)` for exactly this reason.
 * Pinned by the U+FEFF / U+0085 / U+00A0 rows of the shared vocabulary. */
function asciiTrim(s: string): string {
  return s.replace(/^[\t\n\v\f\r ]+/, "").replace(/[\t\n\v\f\r ]+$/, "");
}

/** The `{y, m, d}` triple `invoice-dates::parseIsoDate` returns. */
type IsoParts = { y: number; m: number; d: number };

/** Whole calendar days between two ISO `YYYY-MM-DD` dates (`a − b`).
 * Both are anchored at UTC midnight so the difference is an exact integer
 * day count (no DST drift) — matching the backend's `time::Date`
 * `whole_days()`. Returns `null` if either string is unreadable per
 * [`parseDeadline`]. */
function dayDiff(aIso: string, bIso: string): number | null {
  const a = parseDeadline(aIso);
  const b = parseDeadline(bIso);
  if (a === null || b === null) return null;
  const aMs = Date.UTC(a.y, a.m - 1, a.d);
  const bMs = Date.UTC(b.y, b.m - 1, b.d);
  return Math.round((aMs - bMs) / 86_400_000);
}

/** Classify a payment deadline into its aging bucket relative to `today`.
 * `todayIso` is ISO `YYYY-MM-DD`.
 *
 * Returns `null` when the deadline is MISSING or unparseable. That is not
 * "we could not classify it" — it is a classification: per operator
 * direction such a row is a settled legacy invoice imported from NAV and
 * is NOT OUTSTANDING, so it belongs in no aging bucket at all. Callers
 * must exclude it, which is exactly what `reports::aging_placement` does
 * backend-side when it returns `None`.
 *
 * The backend excludes those rows from the outstanding TOTAL as well as
 * from the buckets, so `sum(buckets) == total` holds; a caller that kept
 * such a row in a list would show the operator an invoice the dashboard
 * says is settled. Mirrors `reports::aging_bucket_for` for every readable
 * deadline, unchanged. */
export function agingBucketFor(
  todayIso: string,
  deadlineIso: string | null | undefined,
): AgingBucket | null {
  const overdue = deadlineIso == null ? null : dayDiff(todayIso, deadlineIso);
  if (overdue === null) return null;
  if (overdue <= 0) return "current";
  if (overdue <= 30) return "d1_30";
  if (overdue <= 60) return "d31_60";
  if (overdue <= 90) return "d61_90";
  return "d90_plus";
}

/** Today's date as a local ISO `YYYY-MM-DD` string. The aging anchor;
 * the report's `period.today` echo uses the same wall-clock day. */
export function todayIsoLocal(): string {
  const d = new Date();
  const y = d.getFullYear();
  const m = `${d.getMonth() + 1}`.padStart(2, "0");
  const day = `${d.getDate()}`.padStart(2, "0");
  return `${y}-${m}-${day}`;
}
