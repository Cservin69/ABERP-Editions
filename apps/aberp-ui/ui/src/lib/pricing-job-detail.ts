// S349 / PR-40 (U1) — pure helpers for the Auto-Quoting row-detail
// panel. Extracted from `PricingJobDetail.svelte` so the
// timeline-derivation, writeback-outcome extraction, and breakdown-row
// shaping can be vitest-pinned without component-render tooling (same
// posture as `pricing-failure-kind.ts` / `pricing-empty-state.ts`).
//
// The detail panel's "Status timeline" and "Last writeback outcome"
// sections are DERIVED from the per-row audit events (the
// `quote_pricing_jobs` row keeps only the current state — audit U20), so
// all three sections read off the one audit page the panel fetches.

import type { AuditEntryView } from "./api";
import type { PricingBreakdownView } from "./api";

/** `serve::audit_view_of` serialises `kind` as the Rust enum's Debug
 *  variant name (e.g. `QuotePricingPosted`), NOT the dotted wire string.
 *  These constants match that representation. */
export const WRITEBACK_OUTCOME_KIND = "QuotePricedWritebackOutcome";

/** Bilingual HU/EN label for an audit-event kind. Covers the
 *  auto-quoting pipeline lifecycle; any other kind (DEAL saga rows that
 *  share the quote id, future kinds) surfaces verbatim so nothing is
 *  silently mislabelled (CLAUDE.md #12). */
export function auditKindLabel(kind: string): string {
  switch (kind) {
    case "QuotePricingFetched":
      return "Beérkezett / Fetched";
    case "QuotePricingExtracted":
      return "CAD-elemzés kész / Extracted";
    case "QuotePricingPriced":
      return "Árazva / Priced";
    case "QuotePricingRendered":
      return "PDF kész / Rendered";
    case "QuotePricingPosted":
      return "Visszaküldve / Posted";
    case "QuotePricingFailed":
      return "Sikertelen / Failed";
    case "QuotePricingFailureClassified":
      return "Hiba besorolva / Failure classified";
    case "QuotePricedWritebackOutcome":
      return "Visszaküldés eredménye / Writeback outcome";
    case "QuotePricingDaemonPanicked":
      return "Daemon összeomlott / Daemon panicked";
    default:
      return kind;
  }
}

/** One node of the derived status timeline. */
export interface TimelineNode {
  occurred_at: string;
  /** Bilingual label from {@link auditKindLabel}. */
  label: string;
  kind: string;
  /** The audit `actor` — `"system"` for daemon-advanced rows, an
   *  operator login for operator-clicked actions. */
  actor: string;
}

/** Map of audit kind → whether it represents a state transition worth a
 *  timeline node. The classification + writeback-outcome rows are
 *  per-attempt diagnostics, not lifecycle transitions, so they're kept
 *  out of the timeline (they surface in the raw-events list + the
 *  dedicated writeback section instead). */
const TIMELINE_KINDS = new Set<string>([
  "QuotePricingFetched",
  "QuotePricingExtracted",
  "QuotePricingPriced",
  "QuotePricingRendered",
  "QuotePricingPosted",
  "QuotePricingFailed",
]);

/** Derive the chronological (oldest-first) status timeline from an audit
 *  page. The backend returns events newest-first; we reverse so the
 *  timeline reads top-to-bottom in pipeline order. */
export function timelineNodes(events: AuditEntryView[]): TimelineNode[] {
  return events
    .filter((e) => TIMELINE_KINDS.has(e.kind))
    .map((e) => ({
      occurred_at: e.occurred_at,
      label: auditKindLabel(e.kind),
      kind: e.kind,
      actor: e.actor,
    }))
    .reverse();
}

/** Structured last-writeback-outcome, lifted from the newest
 *  `QuotePricedWritebackOutcome` audit event's payload. `null` when no
 *  writeback has been attempted yet (the row never reached PostingBack). */
export interface WritebackOutcomeDetail {
  outcome: string;
  http_status: number | null;
  content_type: string | null;
  body_excerpt: string | null;
  retryable: boolean;
  attempt_n: number | null;
  occurred_at: string;
}

/** Find the newest `QuotePricedWritebackOutcome` event and extract its
 *  structured fields. `events` are newest-first, so the first match is
 *  the latest attempt. Returns `null` when none is present on this page
 *  (no writeback attempted, or it's beyond the fetched page). */
export function latestWritebackOutcome(
  events: AuditEntryView[],
): WritebackOutcomeDetail | null {
  const ev = events.find((e) => e.kind === WRITEBACK_OUTCOME_KIND);
  if (!ev) return null;
  const p = (ev.payload ?? {}) as Record<string, unknown>;
  const numOrNull = (v: unknown): number | null =>
    typeof v === "number" ? v : null;
  const strOrNull = (v: unknown): string | null =>
    typeof v === "string" ? v : null;
  return {
    outcome: typeof p.outcome === "string" ? p.outcome : "unknown",
    http_status: numOrNull(p.http_status),
    content_type: strOrNull(p.content_type),
    body_excerpt: strOrNull(p.body_excerpt),
    retryable: p.retryable === true,
    attempt_n: numOrNull(p.attempt_n),
    occurred_at: ev.occurred_at,
  };
}

/** One row of the pricing-breakdown table. */
export interface BreakdownRow {
  /** Bilingual HU/EN label. */
  label: string;
  /** EUR amount. */
  value: number;
}

/** Shape the monetary lines of a breakdown into ordered table rows.
 *  Only the fields actually present are emitted (a future engine schema
 *  that drops a line won't render a `0.00` phantom). Non-monetary fields
 *  (minutes, flags, log) are rendered separately by the component. */
export function breakdownRows(
  breakdown: PricingBreakdownView | null,
): BreakdownRow[] {
  if (!breakdown) return [];
  const lines: Array<[keyof PricingBreakdownView, string]> = [
    ["material_cost", "Anyagköltség / Material"],
    ["labor_cost", "Munkadíj / Labor"],
    ["setup_cost", "Beállítás / Setup"],
    ["overhead", "Rezsi / Overhead"],
    ["margin", "Árrés / Margin"],
    ["total_price", "Végösszeg / Total"],
  ];
  const out: BreakdownRow[] = [];
  for (const [key, label] of lines) {
    const v = breakdown[key];
    if (typeof v === "number") out.push({ label, value: v });
  }
  return out;
}

/** S404 — the engine's `reasoning_log` ("how we priced this"), in full.
 *  The operator (and the customer-facing PDF) must see EVERY line the
 *  engine produced — no top-N cap, no "N more…" tail. This helper is the
 *  single seam the component renders from, so a future accidental
 *  `.slice(0, 5)` cap is a one-line regression caught by vitest rather
 *  than silently hidden from the operator (hulye-biztos). Returns `[]`
 *  for a null/absent breakdown or empty log. */
export function reasoningLogLines(
  breakdown: PricingBreakdownView | null,
): string[] {
  return breakdown?.reasoning_log ?? [];
}
