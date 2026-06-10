// S266 / PR-255 — pure helpers for the Settings → Material Catalogue
// page: the closed-vocab stock-status labels/tones and the sortable-table
// comparator. Kept out of the .svelte component so it is unit-testable
// without a DOM (mirrors lib/invoice-list.ts + lib/adapter-format.ts).

import type {
  CataloguePushStatus,
  QuotingMaterial,
  StockStatus,
} from "./api";

/** Display order of the closed-vocab stock status (kept in sync with the
 * Rust `StockStatus::ALL`). */
export const STOCK_STATUS_ORDER: readonly StockStatus[] = [
  "in_stock",
  "source_1_2d",
  "source_3_7d",
  "special_order",
];

const STOCK_STATUS_LABELS: Record<StockStatus, string> = {
  in_stock: "Raktáron / In stock",
  source_1_2d: "Beszerzés 1–2 nap / Source 1–2d",
  source_3_7d: "Beszerzés 3–7 nap / Source 3–7d",
  special_order: "Egyedi rendelés / Special order",
};

/** Operator-facing label for a stock status; degrades to the raw value
 * (and warns) on an unknown string rather than rendering blank. */
export function stockStatusLabel(s: string): string {
  if (s in STOCK_STATUS_LABELS) {
    return STOCK_STATUS_LABELS[s as StockStatus];
  }
  console.warn(`unknown stock_status: ${s}`);
  return s;
}

/** Chip tone for a stock status — maps to the `.mat-chip--<tone>` classes,
 * which resolve to tokens.css signal colours. */
export function stockStatusTone(
  s: string,
): "positive" | "warning" | "muted" | "neutral" {
  switch (s) {
    case "in_stock":
      return "positive";
    case "source_1_2d":
      return "neutral";
    case "source_3_7d":
      return "warning";
    case "special_order":
      return "muted";
    default:
      return "muted";
  }
}

// ── Sorting ───────────────────────────────────────────────────────────

export type SortKey =
  | "grade"
  | "display_name"
  | "density_g_cm3"
  | "cost_per_kg_eur"
  | "machinability_index"
  | "carbide_life_multiplier"
  | "stock_status"
  | "lead_time_default_days"
  | "quote_multiplier"
  | "updated_at";

export type SortDir = "asc" | "desc";

export interface SortState {
  key: SortKey | null;
  dir: SortDir;
}

/** Three-click cycle: (unsorted) → asc → desc → (unsorted). Pure: returns
 * the next state, mirroring InvoiceList's `onSortClick`. */
export function toggleSort(prev: SortState, key: SortKey): SortState {
  if (prev.key !== key) return { key, dir: "asc" };
  if (prev.dir === "asc") return { key, dir: "desc" };
  return { key: null, dir: "asc" };
}

const NUMERIC_KEYS: ReadonlySet<SortKey> = new Set<SortKey>([
  "density_g_cm3",
  "cost_per_kg_eur",
  "machinability_index",
  "carbide_life_multiplier",
  "lead_time_default_days",
  "quote_multiplier",
]);

function rawCompare(a: QuotingMaterial, b: QuotingMaterial, key: SortKey): number {
  if (key === "stock_status") {
    // Sort by the sourcing tier order, not alphabetically.
    return (
      STOCK_STATUS_ORDER.indexOf(a.stock_status) -
      STOCK_STATUS_ORDER.indexOf(b.stock_status)
    );
  }
  if (NUMERIC_KEYS.has(key)) {
    return (a[key] as number) - (b[key] as number);
  }
  // string keys: grade, display_name, updated_at (RFC3339 sorts lexically)
  return String(a[key]).localeCompare(String(b[key]));
}

/** Stable comparator. Ties break on `grade` ascending so re-sorts are
 * deterministic across refreshes. */
export function compareMaterials(
  a: QuotingMaterial,
  b: QuotingMaterial,
  key: SortKey,
  dir: SortDir,
): number {
  const cmp = rawCompare(a, b, key);
  if (cmp !== 0) return dir === "asc" ? cmp : -cmp;
  return a.grade.localeCompare(b.grade);
}

/** Apply a sort state to a copy of the rows (never mutates input). With no
 * sort key, returns the backend's grade-ascending order untouched. */
export function sortMaterials(
  rows: readonly QuotingMaterial[],
  sort: SortState,
): QuotingMaterial[] {
  const out = [...rows];
  if (sort.key === null) return out;
  const key = sort.key;
  out.sort((a, b) => compareMaterials(a, b, key, sort.dir));
  return out;
}

// ── Catalogue push-status truth (S339 / PR-24) ───────────────────────
//
// The Maintenance dashboard's Material-catalogue tile used to show only
// the grade COUNT — silent about whether the storefront push actually
// works. When the push fails every cycle (the S338/S339 pilot blocker)
// the operator saw a healthy-looking count and no hint of the outage.
// This derives an honest one-line suffix from the live
// `CataloguePushStatus` (same struct the Rust daemon records each
// cycle). `nowMs` is injected so the 30-minute freshness window is
// deterministically testable.

/** Tone for the push-status suffix — drives the chip colour upstream. */
export type PushStatusTone = "positive" | "warning" | "muted";

export interface PushStatusSuffix {
  text: string;
  tone: PushStatusTone;
}

const PUSH_FRESH_WINDOW_MS = 30 * 60 * 1000;

/** Derive the operator-facing push-status suffix from the live status.
 *
 * - success within 30 min → `Pushed to storefront ✓` (positive)
 * - paused on a 401 (rotated bearer) → re-paste prompt (warning)
 * - any non-success outcome → `Push failing — see operator log ⚠` (warning)
 * - dormant (storefront not configured) → muted, non-alarming
 * - never attempted / stale success → `Pending push` (muted)
 */
export function renderPushStatusSuffix(
  status: CataloguePushStatus,
  nowMs: number,
): PushStatusSuffix {
  // A rotated bearer is its own actionable state regardless of outcome.
  if (status.paused) {
    return {
      text: "Push paused — re-paste bearer ⚠",
      tone: "warning",
    };
  }
  const outcome = status.last_outcome;
  if (outcome === null) {
    return { text: "Pending push", tone: "muted" };
  }
  if (outcome === "dormant") {
    // Not an error: the operator simply hasn't configured the storefront.
    return { text: "Storefront not configured", tone: "muted" };
  }
  if (outcome === "ok") {
    const at = status.last_attempt_at ? Date.parse(status.last_attempt_at) : NaN;
    const fresh = Number.isFinite(at) && nowMs - at <= PUSH_FRESH_WINDOW_MS;
    if (fresh) {
      return { text: "Pushed to storefront ✓", tone: "positive" };
    }
    // A stale success means the daemon hasn't run recently (e.g. just
    // booted, backing off, or idle) — honest "pending" rather than a
    // green tick we can't currently vouch for.
    return { text: "Pending push", tone: "muted" };
  }
  // transport / unexpected_status / unauthorized (non-paused) — the
  // push is actively failing. Point the operator at the log.
  return { text: "Push failing — see operator log ⚠", tone: "warning" };
}
