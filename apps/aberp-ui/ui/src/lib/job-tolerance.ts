// T5 / ADR-0097 Part 2 — pure-module helpers for the operator per-job
// tolerance editor (overall spec + per-critical-feature callouts) in the
// quote-intake detail panel. Mirrors `gear-processes.ts`: the draft model +
// the wire↔draft mappers + the closed-vocab dropdown vocabs live here so
// vitest can pin the round-trip without mounting PricingJobDetail.svelte.

import type {
  FeatureTolerance,
  GeneralClassDb,
  ToleranceBody,
  ToleranceSpec,
} from "./api";

/** T5 — the spec-kind dropdown vocab (the five drawing dialects + the inert
 * default). `unspecified` defers to the engine's resolved target (today's
 * behaviour); `per_drawing` raises the manual-review flag. */
export const TOLERANCE_SPEC_KINDS: {
  value: ToleranceSpec["kind"];
  label: string;
}[] = [
  { value: "unspecified", label: "Nincs / Unspecified (default)" },
  { value: "general_class", label: "ISO 2768 osztály / class" },
  { value: "it_grade", label: "IT fokozat / grade" },
  { value: "plus_minus", label: "± mm" },
  { value: "per_drawing", label: "Rajz szerint / Per drawing" },
];

/** T5 — the ISO 2768 general-class dropdown vocab. */
export const GENERAL_CLASSES: { value: GeneralClassDb; label: string }[] = [
  { value: "iso2768_fine", label: "f — fine / finom" },
  { value: "iso2768_medium", label: "m — medium / közepes" },
  { value: "iso2768_coarse", label: "c — coarse / durva" },
  { value: "iso2768_very_coarse", label: "v — very coarse / nagyon durva" },
];

/** T5 — editor draft for one tolerance spec. Inputs are string/literal typed
 * for the DOM `bind:value` seam; only the slot for the selected `kind` is read
 * by [`composeSpec`]. */
export interface SpecDraft {
  kind: ToleranceSpec["kind"];
  generalClass: GeneralClassDb;
  itGrade: string;
  valueMm: string;
}

/** T5 — one per-critical-feature callout draft (a feature index + its spec). */
export interface FeatureToleranceDraft {
  featureIndex: string;
  spec: SpecDraft;
}

/** T5 — the whole editor's draft state: the overall spec + the callouts. */
export interface ToleranceEditorState {
  overall: SpecDraft;
  criticalFeatures: FeatureToleranceDraft[];
}

/** T5 — a fresh spec draft (inert `unspecified`, with sensible per-kind
 * defaults so switching the dropdown shows a usable value). */
export function emptySpecDraft(): SpecDraft {
  return {
    kind: "unspecified",
    generalClass: "iso2768_medium",
    itGrade: "7",
    valueMm: "0.01",
  };
}

/** T5 — fold a wire `ToleranceSpec` into a draft (reverse of [`composeSpec`]).
 * The per-kind slots seed the neutral defaults for the kinds not present. */
export function specToDraft(spec: ToleranceSpec): SpecDraft {
  const d = emptySpecDraft();
  d.kind = spec.kind;
  if (spec.kind === "general_class") d.generalClass = spec.class;
  else if (spec.kind === "it_grade") d.itGrade = String(spec.grade);
  else if (spec.kind === "plus_minus") d.valueMm = String(spec.value_mm);
  return d;
}

/** T5 — compose a draft into the wire `ToleranceSpec`. Numeric strings parse
 * via `parseInt`/`parseFloat` (an unparseable value yields `NaN`, which the
 * backend validator rejects with a typed error). Pure + total. */
export function composeSpec(draft: SpecDraft): ToleranceSpec {
  switch (draft.kind) {
    case "general_class":
      return { kind: "general_class", class: draft.generalClass };
    case "it_grade":
      return { kind: "it_grade", grade: parseInt(draft.itGrade, 10) };
    case "plus_minus":
      return { kind: "plus_minus", value_mm: parseFloat(draft.valueMm) };
    case "per_drawing":
      return { kind: "per_drawing" };
    case "unspecified":
    default:
      return { kind: "unspecified" };
  }
}

/** T5 — a fresh, fully-inert editor state. */
export function emptyToleranceEditorState(): ToleranceEditorState {
  return { overall: emptySpecDraft(), criticalFeatures: [] };
}

/** T5 — seed the editor from the persisted `tolerance_spec_json` column. A
 * `null`/empty/`"{}"`/unparseable blob ⇒ the inert empty state (the same
 * deny-default the pipeline applies). Pure + total. */
export function parseToleranceSpecJson(
  json: string | null,
): ToleranceEditorState {
  if (!json || json.trim() === "" || json.trim() === "{}") {
    return emptyToleranceEditorState();
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return emptyToleranceEditorState();
  }
  if (typeof parsed !== "object" || parsed === null) {
    return emptyToleranceEditorState();
  }
  const obj = parsed as {
    overall?: ToleranceSpec;
    critical_features?: FeatureTolerance[];
  };
  const overall = obj.overall ? specToDraft(obj.overall) : emptySpecDraft();
  const criticalFeatures = Array.isArray(obj.critical_features)
    ? obj.critical_features.map((ft) => ({
        featureIndex: String(ft.feature_index ?? 0),
        spec: ft.spec ? specToDraft(ft.spec) : emptySpecDraft(),
      }))
    : [];
  return { overall, criticalFeatures };
}

/** T5 — compose the editor state into the wire `ToleranceBody` the route
 * accepts (`{ overall, critical_features }`). A non-integer/negative feature
 * index coerces to 0 (the editor sources it from a feature dropdown, so this
 * is a defensive floor). Pure. */
export function composeToleranceBody(
  state: ToleranceEditorState,
): ToleranceBody {
  return {
    overall: composeSpec(state.overall),
    critical_features: state.criticalFeatures.map((f) => {
      const idx = parseInt(f.featureIndex, 10);
      return {
        feature_index: Number.isInteger(idx) && idx >= 0 ? idx : 0,
        spec: composeSpec(f.spec),
      };
    }),
  };
}

/** T5 — `true` when the editor carries no signal (overall `unspecified` AND no
 * callouts) ⇒ saving clears the override back to inert (byte-identical
 * pricing). Mirrors the route's `tolerance_body_is_inert`. */
export function toleranceEditorIsInert(state: ToleranceEditorState): boolean {
  return (
    state.overall.kind === "unspecified" &&
    state.criticalFeatures.length === 0
  );
}
