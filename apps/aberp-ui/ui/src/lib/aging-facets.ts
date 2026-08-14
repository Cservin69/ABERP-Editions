// The three drill-down predicates behind the Finance dashboard's
// clickable tiles, extracted from `InvoiceList.svelte` /
// `IncomingInvoiceList.svelte` so they can be tested by BEHAVIOUR rather
// than by grepping component source.
//
// Why that matters here specifically: each aging tile deep-links into one
// of the two lists via `?aging=`, and the list re-runs the dashboard's
// classification client-side. Under the settled-exclusion rule
// (`reports::aging_placement`) a row with no readable `payment_deadline`
// is not merely bucketed differently — it is off the books entirely, out
// of the outstanding total, every bucket and the past-deadline counters.
// So a list that disagrees with the tile is not a cosmetic drift; it
// shows the operator invoices the dashboard says are settled, or hides
// ones it says are owed.
//
// Pure: no Svelte runes, no DOM, no fetch. `todayIso` is injected rather
// than read from the clock so the pins are deterministic.

import { agingBucketFor, parseDeadline, type AgingBucket } from "./aging";

/** The fields the outgoing (AR) aging predicate reads. Structural, so a
 * full `InvoiceListItem` satisfies it. */
export interface OutgoingAgingRow {
  payment: unknown | null;
  is_storno: boolean;
  state: string;
  payment_deadline: string | null;
}

/** The fields the incoming (AP) predicates read. */
export interface IncomingAgingRow {
  local_status: string;
  payment_deadline: string | null;
}

/** Revenue-recognised states standing in for the backend's "counted"
 * classification. Storno/amended BASE rows (a rarer edge) are not
 * modelled, so the list count can diverge slightly from the dashboard
 * bucket count there — the same best-effort posture S227 documented. */
const COUNTED_STATES: ReadonlySet<string> = new Set(["Submitted", "Recovered", "Finalized"]);

/** AR aging drill-down. Mirrors the backend `reports::aggregate_outgoing`
 * receivables-aging gate: a counted invoice that is unpaid and is not a
 * storno child, classified by `payment_deadline` into the clicked bucket.
 *
 * A `facet` of `null` means "no aging filter active" → every row passes.
 *
 * A row with no READABLE deadline never matches any bucket, because
 * `agingBucketFor` returns `null` for it — the backend treats such a row
 * as a settled legacy import and keeps it out of `receivables_aging` AND
 * out of the receivables total. */
export function outgoingAgingMatches(
  row: OutgoingAgingRow,
  facet: AgingBucket | null,
  todayIso: string,
): boolean {
  if (facet === null) return true;
  if (row.payment !== null) return false; // paid → not a receivable
  if (row.is_storno) return false; // storno child
  if (!COUNTED_STATES.has(row.state)) return false;
  return agingBucketFor(todayIso, row.payment_deadline) === facet;
}

/** AP aging drill-down. Mirrors the backend `reports::aggregate_ap`
 * payables-aging gate: Outstanding rows only, bucketed the same way.
 *
 * Load-bearing on this side — `ap_sync` records no deadline on
 * NAV-synced payables, so unreadable-deadline rows are most of the AP
 * book and including them here would fill a drill-down whose tile reads
 * zero. */
export function incomingAgingMatches(
  inv: IncomingAgingRow,
  facet: AgingBucket | null,
  todayIso: string,
): boolean {
  if (facet === null) return true;
  if (inv.local_status !== "Outstanding") return false;
  return agingBucketFor(todayIso, inv.payment_deadline) === facet;
}

/** AP past-deadline HYGIENE drill-down. Mirrors the backend's
 * `payable_past_deadline_count`: Outstanding AND a deadline that is
 * READABLE and strictly before today.
 *
 * The readability gate is not decoration. This predicate previously
 * compared `inv.payment_deadline >= todayIso` as raw strings after only
 * a `!== null` check, so an unreadable-but-present deadline was ordered
 * LEXICOGRAPHICALLY — `"2026-02-30"` sorts before `"2026-08-13"` and so
 * counted as past-deadline, while the backend classified the same row
 * settled and left it out of the counter. Going through `parseDeadline`
 * makes the two agree: unreadable means settled, on both sides.
 *
 * Comparing the canonical `YYYY-MM-DD` strings after that gate is exact —
 * zero-padded ISO dates sort chronologically. */
export function incomingPastDeadlineMatches(inv: IncomingAgingRow, todayIso: string): boolean {
  if (inv.local_status !== "Outstanding") return false;
  const parsed = parseDeadline(inv.payment_deadline);
  if (parsed === null) return false;
  const canonical = `${parsed.y}-${String(parsed.m).padStart(2, "0")}-${String(parsed.d).padStart(2, "0")}`;
  return canonical < todayIso;
}
