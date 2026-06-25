// S6 / ADR-0094 Gap 3 — pure-module helpers for the SPA's gear-process
// catalogue screen (per-process time coefficients) AND the per-quote gear-op
// preview. Mirrors `machine-rates.ts`: the composer + the wire→form mapper +
// the closed-vocab dropdowns live here so vitest can pin them without
// mounting a Svelte component.

import type {
  GearKindDb,
  GearProcessConcrete,
  GearProcessDb,
  GearProcessRate,
  GearProcessRateInput,
  MachineFamily,
} from "./api";
import { parseMachineValidationError } from "./machines";

// Reuse the shared A157 validation-envelope parser (the backend maps gear-
// process write errors through the same `tunable_write_response`).
export { parseMachineValidationError as parseGearProcessValidationError };

/** S6 — the five concrete (rateable) gear processes for the rates-form
 * dropdown. `auto` is deliberately absent — it is a per-op directive the
 * engine resolves, never a catalogue row. */
export const GEAR_PROCESSES: { value: GearProcessConcrete; label: string }[] = [
  { value: "hob", label: "Hob — external (standalone)" },
  { value: "power_skive", label: "Power-skive — external (in-cycle)" },
  { value: "shape", label: "Shape — internal" },
  { value: "broach", label: "Broach — internal" },
  { value: "wire_edm", label: "Wire-EDM — internal" },
];

/** S6 — gear-op kind dropdown vocab. */
export const GEAR_KINDS: { value: GearKindDb; label: string }[] = [
  { value: "external_spur_helical", label: "External spur / helical" },
  { value: "internal_ring", label: "Internal ring" },
];

/** S6 — gear-op process dropdown vocab. Includes `auto` (engine selects). */
export const GEAR_OP_PROCESSES: { value: GearProcessDb; label: string }[] = [
  { value: "auto", label: "Auto (engine selects)" },
  ...GEAR_PROCESSES,
];

/** S6 — human label for a process db-string (falls back to the raw token). */
export function gearProcessLabel(process: string): string {
  return (
    GEAR_OP_PROCESSES.find((p) => p.value === process)?.label ?? process
  );
}

/** S6 — engine `GEAR_INTERNAL_WIRE_EDM_AGMA`: internal rings escalate from
 * shape to wire-EDM *strictly above* this AGMA class. Kept in lockstep with
 * the engine constant (re-exported in the S5 handoff). */
export const GEAR_INTERNAL_WIRE_EDM_AGMA = 12;

/** S6 — pure TS mirror of `aberp_quote_engine::select_gear_process`, for the
 * live per-op preview (same posture as the `route_family` / `is_exotic_material`
 * previews). External → power-skive on a routed Swiss/turn-mill (in-cycle),
 * else hob; internal → shape, escalating to wire-EDM strictly above AGMA 12.
 * Deterministic + total. Pinned against the engine's documented behaviour by
 * `gear-processes.test.ts`. */
export function selectGearProcess(
  kind: GearKindDb,
  routedFamily: MachineFamily,
  qualityAgma: number,
): GearProcessConcrete {
  if (kind === "external_spur_helical") {
    return routedFamily === "swiss-turn-mill" || routedFamily === "turn-mill"
      ? "power_skive"
      : "hob";
  }
  // internal_ring
  return qualityAgma > GEAR_INTERNAL_WIRE_EDM_AGMA ? "wire_edm" : "shape";
}

/** S6 — operator-typed form state for the GearProcessForm modal. Numeric
 * slots are string-valued so the DOM `bind:value` round-trips cleanly;
 * `process` is the closed-vocab dropdown's selected literal. */
export interface GearProcessFormState {
  process: GearProcessConcrete;
  setupMin: string;
  minPerTooth: string;
  moduleExponent: string;
  agmaQualityFactorBase: string;
  inCycleFactor: string;
  notes: string;
}

/** S6 — defaults for a freshly-opened GearProcessForm in create mode: the
 * external hobbing process with the day-1 seed coefficients. */
export function emptyGearProcessForm(): GearProcessFormState {
  return {
    process: "hob",
    setupMin: "20",
    minPerTooth: "0.30",
    moduleExponent: "1.0",
    agmaQualityFactorBase: "0.10",
    inCycleFactor: "1.0",
    notes: "",
  };
}

/** S6 — fold a fetched rate into edit-mode form state (numeric fields
 * stringify so the `<input bind:value>` seam stays typed-as-string). The
 * reverse direction is [`composeGearProcessInputs`]. */
export function formFromGearProcess(r: GearProcessRate): GearProcessFormState {
  return {
    process: r.process,
    setupMin: String(r.setup_min),
    minPerTooth: String(r.min_per_tooth),
    moduleExponent: String(r.module_exponent),
    agmaQualityFactorBase: String(r.agma_quality_factor_base),
    inCycleFactor: String(r.in_cycle_factor),
    notes: r.notes ?? "",
  };
}

/** S6 — turn the form state into the wire `GearProcessRateInput` body. Pure;
 * the numeric strings parse via `parseFloat` (an unparseable value yields
 * `NaN`, which the backend validator rejects with a typed field error the
 * form renders inline). */
export function composeGearProcessInputs(
  form: GearProcessFormState,
): GearProcessRateInput {
  return {
    process: form.process,
    setup_min: parseFloat(form.setupMin),
    min_per_tooth: parseFloat(form.minPerTooth),
    module_exponent: parseFloat(form.moduleExponent),
    agma_quality_factor_base: parseFloat(form.agmaQualityFactorBase),
    in_cycle_factor: parseFloat(form.inCycleFactor),
    notes: form.notes.trim() === "" ? null : form.notes.trim(),
  };
}
