// ADR-0112 Part C (D-19 slice C2/C3) — pure-module helpers for the SPA's
// drilling-rate catalogue screen (per-material-group drilling cycle-time
// coefficients). Mirrors `machine-rates.ts`: the wire↔form mappers and the
// list-chip helpers live here so vitest can pin them without mounting a
// Svelte component.

import type { DrillingRate, DrillingRateInput } from "./api";
import { parseMachineValidationError } from "./machines";

// Reuse the shared validation-error parser (the backend emits the same
// `{ error: "validation_failed", fields: [...] }` shape for every quoting
// tunable).
export { parseMachineValidationError as parseDrillingRateValidationError };

/** Operator-typed form state for the DrillingRateForm modal. Numeric slots
 * are string-valued so the DOM `bind:value` round-trips cleanly. */
export interface DrillingRateFormState {
  materialGroup: string;
  feedMmPerMinPerMmDia: string;
  peckDepthDiaMultiple: string;
  peckRetractSec: string;
  rapidPerHoleSec: string;
  toolChangeSec: string;
  flatBottomFactor: string;
  unknownEndConditionFactor: string;
  notes: string;
}

/** Defaults for a freshly-opened DrillingRateForm in create mode: an INERT
 * row (feed 0) with sensible placeholder kinematics, so the operator's only
 * required decision is the material group + the real feed that switches
 * pricing on. */
export function emptyDrillingRateForm(): DrillingRateFormState {
  return {
    materialGroup: "",
    feedMmPerMinPerMmDia: "0",
    peckDepthDiaMultiple: "3",
    peckRetractSec: "0.6",
    rapidPerHoleSec: "2",
    toolChangeSec: "20",
    flatBottomFactor: "1.2",
    unknownEndConditionFactor: "1.5",
    notes: "",
  };
}

/** Fold a fetched rate into edit-mode form state. Numeric fields stringify so
 * the `<input bind:value>` seam stays typed-as-string. Reverse:
 * [`composeDrillingRateInputs`]. */
export function formFromDrillingRate(rate: DrillingRate): DrillingRateFormState {
  return {
    materialGroup: rate.material_group,
    feedMmPerMinPerMmDia: String(rate.feed_mm_per_min_per_mm_dia),
    peckDepthDiaMultiple: String(rate.peck_depth_dia_multiple),
    peckRetractSec: String(rate.peck_retract_sec),
    rapidPerHoleSec: String(rate.rapid_per_hole_sec),
    toolChangeSec: String(rate.tool_change_sec),
    flatBottomFactor: String(rate.flat_bottom_factor),
    unknownEndConditionFactor: String(rate.unknown_end_condition_factor),
    notes: rate.notes ?? "",
  };
}

/** Turn the form state into the wire `DrillingRateInput` body. Pure; numeric
 * strings parse via `parseFloat` (an unparseable value yields `NaN`, which the
 * backend validator rejects with a typed field error the form renders
 * inline). The material group is trimmed. */
export function composeDrillingRateInputs(
  form: DrillingRateFormState,
): DrillingRateInput {
  return {
    material_group: form.materialGroup.trim(),
    feed_mm_per_min_per_mm_dia: parseFloat(form.feedMmPerMinPerMmDia),
    peck_depth_dia_multiple: parseFloat(form.peckDepthDiaMultiple),
    peck_retract_sec: parseFloat(form.peckRetractSec),
    rapid_per_hole_sec: parseFloat(form.rapidPerHoleSec),
    tool_change_sec: parseFloat(form.toolChangeSec),
    flat_bottom_factor: parseFloat(form.flatBottomFactor),
    unknown_end_condition_factor: parseFloat(form.unknownEndConditionFactor),
    notes: form.notes.trim() === "" ? null : form.notes.trim(),
  };
}

/** A rate is INERT when its feed is not a positive number — the engine's
 * `drilling_active` predicate (`feed > 0`) never matches it, so no drilling is
 * priced for that material. Both the seed default (feed 0) and a hand-cleared
 * feed land here. */
export function isInertDrillingRate(rate: DrillingRate): boolean {
  return !(rate.feed_mm_per_min_per_mm_dia > 0);
}

/** The status-chip text for a rate row: an inert row says so loudly (it prices
 * nothing), an active row shows its feed. Pure; pinned by
 * `drilling-rates.test.ts`. */
export function drillingRateStatusLabel(rate: DrillingRate): string {
  if (isInertDrillingRate(rate)) return "inert — no drilling priced";
  return `active — feed ${rate.feed_mm_per_min_per_mm_dia} mm/min per mm ⌀`;
}
