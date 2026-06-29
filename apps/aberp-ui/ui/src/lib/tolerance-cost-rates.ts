// T5 / ADR-0097 Part 2 — pure-module helpers for the SPA's tolerance
// cost-rate catalogue screen (per-band machining-tolerance cost drivers).
// Mirrors `machine-rates.ts`: the composer + the wire→form mapper + the
// closed-vocab band list live here so vitest can pin them without mounting a
// Svelte component.

import type {
  ToleranceCostRate,
  ToleranceCostRateInput,
  ToleranceRange,
} from "./api";
import { TOLERANCE_RANGES } from "./api";
import { toleranceRangeLabel } from "./quoting-tunables-format";
import { parseMachineValidationError } from "./machines";

// Reuse the shared A157 validation-envelope parser (the backend maps tolerance
// cost-rate write errors through the same `tunable_write_response`).
export { parseMachineValidationError as parseToleranceCostRateValidationError };

/** T5 — the five governing bands for the rate-form dropdown, in tightness
 * order, each with its friendly label. */
export const TOLERANCE_BANDS: { value: ToleranceRange; label: string }[] =
  TOLERANCE_RANGES.map((b) => ({ value: b, label: toleranceRangeLabel(b) }));

/** T5 — operator-typed form state for the ToleranceCostRateForm modal. Numeric
 * slots are string-valued so the DOM `bind:value` round-trips cleanly; the
 * band is the closed-vocab dropdown's selected literal. */
export interface ToleranceCostRateFormState {
  toleranceClass: ToleranceRange;
  finishPassesAdd: string;
  inprocInspectionMin: string;
  cmmMinPerCriticalFeature: string;
  reworkScrapPct: string;
  feedSlowdownFactor: string;
  grindingEscalation: boolean;
  notes: string;
}

/** T5 — defaults for a freshly-opened form in create mode: the Standard band
 * with the zero-contribution seed values (neutral 1.0 feed factor; no
 * grinding). A row created with these moves no money — the operator tunes
 * upward from there (ADR-0097 R4). */
export function emptyToleranceCostRateForm(): ToleranceCostRateFormState {
  return {
    toleranceClass: "standard",
    finishPassesAdd: "0",
    inprocInspectionMin: "0",
    cmmMinPerCriticalFeature: "0",
    reworkScrapPct: "0",
    feedSlowdownFactor: "1.0",
    grindingEscalation: false,
    notes: "",
  };
}

/** T5 — fold a fetched rate into edit-mode form state (numeric fields
 * stringify so the `<input bind:value>` seam stays typed-as-string). The
 * reverse direction is [`composeToleranceCostRateInputs`]. */
export function formFromToleranceCostRate(
  rate: ToleranceCostRate,
): ToleranceCostRateFormState {
  return {
    toleranceClass: rate.tolerance_class,
    finishPassesAdd: String(rate.finish_passes_add),
    inprocInspectionMin: String(rate.inproc_inspection_min),
    cmmMinPerCriticalFeature: String(rate.cmm_min_per_critical_feature),
    reworkScrapPct: String(rate.rework_scrap_pct),
    feedSlowdownFactor: String(rate.feed_slowdown_factor),
    grindingEscalation: rate.grinding_escalation,
    notes: rate.notes ?? "",
  };
}

/** T5 — turn the form state into the wire `ToleranceCostRateInput` body. Pure;
 * the numeric strings parse via `parseFloat` (an unparseable value yields
 * `NaN`, which the backend validator rejects with a typed field error the form
 * renders inline). */
export function composeToleranceCostRateInputs(
  form: ToleranceCostRateFormState,
): ToleranceCostRateInput {
  return {
    tolerance_class: form.toleranceClass,
    finish_passes_add: parseFloat(form.finishPassesAdd),
    inproc_inspection_min: parseFloat(form.inprocInspectionMin),
    cmm_min_per_critical_feature: parseFloat(form.cmmMinPerCriticalFeature),
    rework_scrap_pct: parseFloat(form.reworkScrapPct),
    feed_slowdown_factor: parseFloat(form.feedSlowdownFactor),
    grinding_escalation: form.grindingEscalation,
    notes: form.notes.trim() === "" ? null : form.notes.trim(),
  };
}

/** T5 — `true` when a rate is the zero-contribution seed (every additive
 * driver 0, the neutral 1.0 feed factor, grinding off) ⇒ it moves no money.
 * The list renders a chip from this so the operator sees at a glance which
 * bands are tuned vs. dormant. Pure. */
export function isZeroContribution(rate: ToleranceCostRate): boolean {
  return (
    rate.finish_passes_add === 0 &&
    rate.inproc_inspection_min === 0 &&
    rate.cmm_min_per_critical_feature === 0 &&
    rate.rework_scrap_pct === 0 &&
    rate.feed_slowdown_factor === 1 &&
    !rate.grinding_escalation
  );
}
