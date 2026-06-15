// S428 — pure-module helpers for the SPA's margin-profiles master-data
// screen. The composer (`composeMarginProfileInputs`), the wire-to-form
// mapper (`formFromProfile`), the client-side filter (`filterProfiles`),
// and the percent helpers all live here so vitest can pin them without
// mounting a Svelte component (mirror of `machines.ts`).
//
// Pinned by `margin-profiles.test.ts`.

import type {
  CustomerType,
  MarginProfile,
  MarginProfileInputs,
} from "./api";
import { CUSTOMER_TYPE_OPTIONS } from "./partners";

/** S428 — operator-typed form state for the MarginProfileForm modal. The
 * percentage slots are string-valued (and entered as PERCENT, e.g. "35"
 * for 35%) so the DOM `bind:value` round-trips cleanly; the composer
 * converts to the wire fraction. */
export interface MarginProfileFormState {
  name: string;
  customerType: CustomerType;
  /** Target gross margin, as a percent string (e.g. "35"). */
  grossMarginPct: string;
  /** Minimum (floor) margin, as a percent string (e.g. "10"). */
  minMarginPct: string;
  notes: string;
  enabled: boolean;
}

/** S428 — customer-type options for the form dropdown, reusing the
 * partner vocab so the two stay in lockstep. */
export const MARGIN_PROFILE_CUSTOMER_TYPES = CUSTOMER_TYPE_OPTIONS;

/** S428 — defaults for a freshly-opened MarginProfileForm. Target 35% /
 * floor 10% mirror the engine's day-1 global defaults; `customerType`
 * starts at the first business segment (a profile keyed `unset` would be
 * meaningless — `unset` buyers always take the global default). */
export function emptyMarginProfileForm(): MarginProfileFormState {
  return {
    name: "",
    customerType: "industrial",
    grossMarginPct: "35",
    minMarginPct: "10",
    notes: "",
    enabled: true,
  };
}

/** S428 — fold a fetched profile into edit-mode form state. The wire
 * fractions (0.35) render as percent strings ("35"). */
export function formFromProfile(p: MarginProfile): MarginProfileFormState {
  return {
    name: p.name,
    customerType: p.customer_type as CustomerType,
    grossMarginPct: fractionToPercentString(p.gross_margin_pct),
    minMarginPct: fractionToPercentString(p.min_margin_pct),
    notes: p.notes ?? "",
    enabled: p.enabled,
  };
}

/** S428 — turn form state into the wire `MarginProfileInputs`. Percent
 * strings parse to fractions (35 → 0.35); an unparseable value yields
 * `NaN`, which the backend validator rejects with a typed field error. */
export function composeMarginProfileInputs(
  form: MarginProfileFormState,
): MarginProfileInputs {
  return {
    name: form.name.trim(),
    customer_type: form.customerType,
    gross_margin_pct: percentStringToFraction(form.grossMarginPct),
    min_margin_pct: percentStringToFraction(form.minMarginPct),
    notes: form.notes.trim().length > 0 ? form.notes.trim() : null,
    enabled: form.enabled,
  };
}

/** S428 — "35" → 0.35. `NaN` propagates for the backend to reject. */
export function percentStringToFraction(s: string): number {
  return parseFloat(s) / 100;
}

/** S428 — 0.35 → "35". Trims trailing zeros for readability. */
export function fractionToPercentString(frac: number): string {
  const pct = frac * 100;
  return Number.isInteger(pct) ? String(pct) : String(Number(pct.toFixed(4)));
}

/** S428 — display a fraction as a percent label, e.g. 0.35 → "35%". */
export function formatPercent(frac: number): string {
  return `${fractionToPercentString(frac)}%`;
}

/** S428 — name / customer-type substring filter for the list. */
export function filterProfiles(
  rows: MarginProfile[],
  needle: string,
): MarginProfile[] {
  const q = needle.trim().toLowerCase();
  if (q.length === 0) return rows;
  return rows.filter(
    (p) =>
      p.name.toLowerCase().includes(q) ||
      p.customer_type.toLowerCase().includes(q),
  );
}

/** S428 — typed 400 validation body parser (mirror of
 * `parseMachineValidationError`). Returns `null` for any other shape. */
export function parseMarginProfileValidationError(
  raw: string,
):
  | { error: "validation_failed"; fields: Array<{ field: string; message: string }> }
  | null {
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start < 0 || end <= start) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.slice(start, end + 1));
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const obj = parsed as Record<string, unknown>;
  if (obj.error !== "validation_failed") return null;
  if (!Array.isArray(obj.fields)) return null;
  const fields: Array<{ field: string; message: string }> = [];
  for (const entry of obj.fields) {
    if (typeof entry !== "object" || entry === null) return null;
    const e = entry as Record<string, unknown>;
    if (typeof e.field !== "string" || typeof e.message !== "string") {
      return null;
    }
    fields.push({ field: e.field, message: e.message });
  }
  return { error: "validation_failed", fields };
}
