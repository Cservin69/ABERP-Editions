// S427 — pure-module helpers for the SPA's quoting-machine master-data
// screen. The composer (`composeMachineInputs`), the wire-to-form
// mapper (`formFromMachine`), the client-side filter (`filterMachines`),
// and the lead-time chip/effective helpers all live here so vitest can
// pin them without mounting a Svelte component (mirror of `partners.ts`).
//
// Pinned by `machines.test.ts`.

import type {
  MachineFamily,
  MachineInputs,
  QuotingMachine,
} from "./api";

/** S427 — operator-typed form state for the MachineForm modal. One
 * field per `MachineInputs` slot; the numeric slots (envelope x/y/z,
 * daily hours, buffer %) are string-valued so the DOM `bind:value`
 * round-trips cleanly. `family` is the closed-vocab dropdown's
 * selected literal; `enabled` binds a checkbox. */
export interface MachineFormState {
  name: string;
  family: MachineFamily;
  envelopeX: string;
  envelopeY: string;
  envelopeZ: string;
  dailyHoursAvail: string;
  bufferPct: string;
  enabled: boolean;
}

/** S427 — closed vocab of machine families with human labels for the
 * form dropdown + the list facet. The `value` strings are the EXACT
 * db-strings the backend serde expects. */
export const MACHINE_FAMILIES: readonly { value: MachineFamily; label: string }[] = [
  { value: "3-axis-mill", label: "3-axis mill" },
  { value: "5-axis-mill", label: "5-axis mill" },
  { value: "wire-EDM", label: "Wire EDM" },
  { value: "sinker-EDM", label: "Sinker EDM" },
  { value: "lathe", label: "Lathe" },
  { value: "grinder", label: "Grinder" },
  { value: "additive", label: "Additive" },
  { value: "other", label: "Other" },
  // S4 / ADR-0094 Gap 2 — turn-mill family extension.
  { value: "swiss-turn-mill", label: "Swiss turn-mill (lights-out)" },
  { value: "turn-mill", label: "Turn-mill" },
  { value: "4-axis-mill", label: "4-axis mill" },
];

/** S427 — human label for a family db-string. Falls back to the raw
 * string for any value the closed vocab doesn't cover (a SPA older
 * than the backend). */
export function machineFamilyLabel(family: string): string {
  const match = MACHINE_FAMILIES.find((f) => f.value === family);
  return match?.label ?? family;
}

/** S427 — defaults for a freshly-opened MachineForm in create mode.
 * Mirrors the backend's `MachineInputs` defaults: `family` defaults to
 * the dominant `3-axis-mill`, `daily_hours_avail=16`, `buffer_pct=20`,
 * `enabled=true`. The envelope defaults to "0"/"0"/"0" — the operator
 * fills the real bounds before save. */
export function emptyMachineForm(): MachineFormState {
  return {
    name: "",
    family: "3-axis-mill",
    envelopeX: "0",
    envelopeY: "0",
    envelopeZ: "0",
    dailyHoursAvail: "16",
    bufferPct: "20",
    enabled: true,
  };
}

/** S427 — fold a fetched machine into edit-mode form state. The
 * numeric fields stringify so the `<input bind:value>` DOM seam stays
 * typed-as-string. The reverse direction is [`composeMachineInputs`]. */
export function formFromMachine(machine: QuotingMachine): MachineFormState {
  return {
    name: machine.name,
    family: machine.family,
    envelopeX: String(machine.max_envelope_xyz_mm[0]),
    envelopeY: String(machine.max_envelope_xyz_mm[1]),
    envelopeZ: String(machine.max_envelope_xyz_mm[2]),
    dailyHoursAvail: String(machine.daily_hours_avail),
    bufferPct: String(machine.buffer_pct),
    enabled: machine.enabled,
  };
}

/** S427 — turn the form state into the wire `MachineInputs` body.
 * Pure; no side effects. Trims `name` so a `"   "` value surfaces as
 * the backend's actionable validation error rather than slipping
 * through. The numeric strings parse via `parseFloat`; an unparseable
 * value yields `NaN` which the backend's validator rejects with a
 * typed field error (the form renders it inline). */
export function composeMachineInputs(form: MachineFormState): MachineInputs {
  return {
    name: form.name.trim(),
    family: form.family,
    max_envelope_xyz_mm: [
      parseFloat(form.envelopeX),
      parseFloat(form.envelopeY),
      parseFloat(form.envelopeZ),
    ],
    daily_hours_avail: parseFloat(form.dailyHoursAvail),
    buffer_pct: parseFloat(form.bufferPct),
    enabled: form.enabled,
  };
}

/** S427 — closed-vocab family facet for the list. `"All"`
 * short-circuits the gate; the eight literal values mirror
 * `MachineFamily`. */
export type MachineFamilyFacet = "All" | MachineFamily;

/** S427 — quick-filter facet spec: a name substring (`needle`) AND a
 * closed-vocab family facet. A row must pass every engaged facet. */
export interface MachineFilterSpec {
  needle: string;
  family: MachineFamilyFacet;
}

/** S427 — empty filter (every facet open). */
export const EMPTY_MACHINE_FILTER: MachineFilterSpec = {
  needle: "",
  family: "All",
};

/** S427 — `true` iff every facet is open. */
export function isMachineFilterEmpty(spec: MachineFilterSpec): boolean {
  return spec.needle.trim().length === 0 && spec.family === "All";
}

/** S427 — name search + family facet filter for the MachinesList. The
 * needle is a case-insensitive substring on `name`; the family facet
 * ANDs on top. Empty / whitespace-only needle + `"All"` family returns
 * the list unchanged. */
export function filterMachines(
  rows: QuotingMachine[],
  spec: MachineFilterSpec,
): QuotingMachine[] {
  const familyGated =
    spec.family === "All"
      ? rows
      : rows.filter((m) => m.family === spec.family);
  const q = spec.needle.trim().toLowerCase();
  if (q.length === 0) return familyGated;
  return familyGated.filter((m) => m.name.toLowerCase().includes(q));
}

/** S427 — effective lead-time: the operator override wins over the
 * engine-computed value; `null` when neither is set. */
export function effectiveLeadTime(
  computed: number | null,
  override: number | null,
): number | null {
  return override ?? computed;
}

/** S427 — categorical colour class for the lead-time chip, keyed on
 * the effective day count. `<= 7` → ok (green), `8..21` → warning
 * (amber), `> 21` → err (red). Pure; pinned at the boundaries by
 * `machines.test.ts`. */
export function leadTimeChipClass(days: number): string {
  if (days <= 7) return "chip chip--ok";
  if (days <= 21) return "chip chip--warning";
  return "chip chip--err";
}

/** S427 — typed 400 validation body parser. Mirror of
 * `parsePartnerValidationError`: peel the JSON object out of the
 * Tauri-wrapped error string, accept iff the `error` discriminant
 * matches `"validation_failed"`. Returns `null` for any other shape so
 * the caller falls back to a generic raw-string display. */
export function parseMachineValidationError(
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
