// PR-172 — pure filter/dedupe helpers for the NotesAutocomplete
// typeahead. Extracted from the Svelte component so the matching
// behaviour is unit-testable without dragging in @testing-library/svelte
// (the SPA's vitest setup has no DOM testing layer; this matches the
// posture taken by PartnerTypeahead.svelte and ProductCombobox, which
// keep their filter logic in companion .ts modules).

import type { NotesHistoryScope } from "./api";

/** PR-172 — re-export the wire scope so component callers don't have
 * to reach into api.ts for the typed union when they only need it as
 * a prop. The brand stays single-source-of-truth in api.ts. */
export type { NotesHistoryScope };

/** PR-172 — filter a list of history strings by what the operator has
 * typed in the textarea so far. Conservative defaults per the PR-172
 * brief:
 *
 *   - **startsWith** match (case-insensitive) — predictable over
 *     fuzzy contains. An operator typing "Köszön" sees
 *     "Köszönjük együttműködését" surface but NOT a stray
 *     "Felmondás — Köszönöm" buried mid-string. Reduces surprise.
 *   - **case-insensitive** comparison only for ranking — the original
 *     casing of each history entry is preserved when surfaced.
 *   - **trim** the operator's typed prefix before comparison so a
 *     leading space does not silently hide every match.
 *   - **empty input** ⇒ return the first `topN` history items as-is
 *     (the operator can browse without typing).
 *
 * No fuzzy ranking, no scoring. Predictability beats cleverness for
 * a buyer-facing-text typeahead. */
export function filterNotesByPrefix(
  history: readonly string[],
  rawInput: string,
  topN: number,
): string[] {
  const cap = Math.max(0, topN | 0);
  if (cap === 0) {
    return [];
  }
  const needle = rawInput.trim().toLocaleLowerCase();
  if (needle.length === 0) {
    return history.slice(0, cap);
  }
  const out: string[] = [];
  for (const candidate of history) {
    if (out.length >= cap) {
      break;
    }
    if (candidate.toLocaleLowerCase().startsWith(needle)) {
      // PR-172 — skip exact-match echo: if the operator has typed
      // the entire candidate verbatim, suggesting it back as a
      // pick-me dropdown row is noisy (the suggestion would replace
      // the typed text with itself). The next keystroke that breaks
      // the equality re-surfaces the row.
      if (candidate === rawInput.trim()) {
        continue;
      }
      out.push(candidate);
    }
  }
  return out;
}
