// S350 / PR-39 (U5) — pure helpers for the operator material-grade
// override in `PricingJobDetail.svelte`. Kept out of the Svelte
// component (which has no render-test harness in this package) so the
// edit gate, the catalogue→options mapping, and the inline-error copy
// are unit-pinned. The component wires straight to these.

import type { MaterialEditError, QuotingMaterial } from "./api";

/** Closed-vocab JobStates in which an operator may inline-edit the
 * material grade. Mirrors `JobState::material_editable` on the Rust
 * side: editable while a row is awaiting/just-failed (Fetched /
 * PostingBack / Failed) and refused mid-pipeline (Extracting / Pricing /
 * Rendering — would race the daemon) or once Posted (terminal — a
 * re-priced grade would 409 on the new hash). */
export const MATERIAL_EDITABLE_STATES: ReadonlySet<string> = new Set([
  "fetched",
  "posting_back",
  "failed",
]);

/** Drives the Edit-pencil visibility: shown only when the row's current
 * state permits an edit. */
export function isMaterialEditable(state: string): boolean {
  return MATERIAL_EDITABLE_STATES.has(state);
}

/** One `<option>` for the grade select. */
export interface MaterialOption {
  /** The catalogue grade — the value PATCHed to the backend. */
  value: string;
  /** Operator-facing label: `display_name (grade)` when a display name
   * exists, else the bare grade. */
  label: string;
}

/** Map the catalogue snapshot (`listQuotingMaterials`) to select
 * options. Preserves the backend's order (grade ASC). A grade with no
 * display name falls back to the bare grade so the option is never
 * blank. */
export function materialOptions(materials: QuotingMaterial[]): MaterialOption[] {
  return materials.map((m) => ({
    value: m.grade,
    label: m.display_name ? `${m.display_name} (${m.grade})` : m.grade,
  }));
}

/** Bilingual HU/EN inline message for a failed material edit, keyed on
 * the typed code. Rendered under the select (400) or next to the
 * Save/Cancel row (409). */
export function materialEditInlineCopy(err: MaterialEditError): string {
  switch (err.code) {
    case "MaterialNotInCatalogue": {
      const n = err.availableCount;
      const count =
        typeof n === "number"
          ? ` (${n} elérhető / ${n} available)`
          : "";
      return `Ez az anyag nincs a katalógusban — válassz a listából${count}. / Material not in the catalogue — pick one from the list${count}.`;
    }
    case "JobNotEditable":
      return "Ez a sor a jelenlegi állapotában nem szerkeszthető (csak Beérkezett / Visszaküldés / Sikertelen állapotban). / This row cannot be edited in its current state (only while Fetched / Posting back / Failed).";
    case "EmptyMaterialGrade":
      return "Válassz egy anyagot. / Pick a material.";
    default:
      return err.message;
  }
}
