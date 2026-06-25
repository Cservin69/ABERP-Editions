// S4 / ADR-0094 Gap 2 — pure-module helpers for the SPA's machine-rate
// catalogue screen (per-family EUR/min + lights-out factor). Mirrors
// `machines.ts`: the composer + the wire→form mapper live here so vitest
// can pin them without mounting a Svelte component.

import type { MachineFamily, MachineRate, MachineRateInput } from "./api";
import {
  MACHINE_FAMILIES,
  machineFamilyLabel,
  parseMachineValidationError,
} from "./machines";

// Re-export the shared family vocab + label + validation parser so the
// machine-rate screen imports from one place.
export {
  MACHINE_FAMILIES,
  machineFamilyLabel,
  parseMachineValidationError as parseMachineRateValidationError,
};

/** S4 — operator-typed form state for the MachineRateForm modal. Numeric
 * slots are string-valued so the DOM `bind:value` round-trips cleanly;
 * `family` is the closed-vocab dropdown's selected literal. */
export interface MachineRateFormState {
  family: MachineFamily;
  attendedRateEurPerMin: string;
  lightsOutFactor: string;
  unattendedCapable: boolean;
  notes: string;
}

/** S4 — defaults for a freshly-opened MachineRateForm in create mode:
 * the lights-out Swiss family, a 1.5 €/min attended rate, neutral
 * factor 1.0, attended-only. */
export function emptyMachineRateForm(): MachineRateFormState {
  return {
    family: "swiss-turn-mill",
    attendedRateEurPerMin: "1.5",
    lightsOutFactor: "1.0",
    unattendedCapable: false,
    notes: "",
  };
}

/** S4 — fold a fetched rate into edit-mode form state. The numeric
 * fields stringify so the `<input bind:value>` seam stays typed-as-string.
 * The reverse direction is [`composeMachineRateInputs`]. */
export function formFromMachineRate(rate: MachineRate): MachineRateFormState {
  return {
    family: rate.family,
    attendedRateEurPerMin: String(rate.attended_rate_eur_per_min),
    lightsOutFactor: String(rate.lights_out_factor),
    unattendedCapable: rate.unattended_capable,
    notes: rate.notes ?? "",
  };
}

/** S4 — turn the form state into the wire `MachineRateInput` body. Pure;
 * the numeric strings parse via `parseFloat` (an unparseable value yields
 * `NaN`, which the backend validator rejects with a typed field error the
 * form renders inline). */
export function composeMachineRateInputs(
  form: MachineRateFormState,
): MachineRateInput {
  return {
    family: form.family,
    attended_rate_eur_per_min: parseFloat(form.attendedRateEurPerMin),
    lights_out_factor: parseFloat(form.lightsOutFactor),
    unattended_capable: form.unattendedCapable,
    notes: form.notes.trim() === "" ? null : form.notes.trim(),
  };
}

/** S4 — render the effective lights-out chip text for a rate row: the
 * unattended families show their discounted €/min, attended-only families
 * show "attended only". Pure; pinned by `machine-rates.test.ts` if added. */
export function effectiveLightsOutLabel(rate: MachineRate): string {
  if (!rate.unattended_capable) return "attended only";
  const eff = rate.attended_rate_eur_per_min * rate.lights_out_factor;
  return `lights-out ${eff.toFixed(4)} €/min`;
}
