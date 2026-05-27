// PR-74 / session-96 — pure-module helper for the IssueInvoice form's
// buyer combobox. Replaces the PR-54 two-input posture (a separate
// `Search saved partners` typeahead above a `Name (auto-filled)`
// input) with a single input whose dropdown surfaces saved-partner
// matches as the operator types.
//
// The helper is intentionally pure (no Svelte runes, no DOM, no
// backend calls) so vitest can pin the pick-vs-type-through invariants
// without mounting a component or stubbing `invoke`. The Svelte
// component owns the partners-list fetch + the keyboard nav; this
// module owns the "given a needle + the loaded list, what does the
// dropdown show" decision.

import type { Partner } from "./api";

/** PR-74 — derived view returned to the combobox renderer.
 *
 * `matches` is the saved-partner subset whose `display_name`,
 * `legal_name`, or `tax_number` contains the (lowercased) needle.
 * Capped at `maxMatches` so a wildcard prefix like "a" cannot blow
 * the dropdown up to 200 rows.
 *
 * `shouldShowDropdown` is `true` once the trimmed needle reaches
 * `minChars`. Distinct from `matches.length > 0` because we want to
 * surface a "no match — will be saved as free-text" hint when the
 * operator types a name that doesn't match any saved partner (rather
 * than silently hiding the dropdown, which would look like the
 * typeahead is broken — the very regression PR-74 closes). */
export interface BuyerComboboxState {
  matches: Partner[];
  shouldShowDropdown: boolean;
}

export interface BuyerComboboxArgs {
  /** Current operator-typed text in the buyer-name input. */
  needle: string;
  /** Full saved-partners list (loaded once on dialog open). The
   * combobox filters client-side; no per-keystroke fetch. */
  savedPartners: Partner[];
  /** Minimum trimmed needle length before the dropdown shows.
   * Defaults to 3 to mirror the PR-54 PartnerTypeahead posture. */
  minChars?: number;
  /** Maximum matches to surface in the dropdown. Defaults to 8 to
   * mirror PR-54. */
  maxMatches?: number;
}

/** PR-74 — given the current input value + the loaded partners list,
 * compute what the dropdown should show. Pure function; pinned by
 * `buyer-combobox.test.ts`. */
export function buyerComboboxState(
  args: BuyerComboboxArgs,
): BuyerComboboxState {
  const minChars = args.minChars ?? 3;
  const maxMatches = args.maxMatches ?? 8;
  const trimmed = args.needle.trim();
  if (trimmed.length < minChars) {
    return { matches: [], shouldShowDropdown: false };
  }
  const q = trimmed.toLowerCase();
  const matches = args.savedPartners
    .filter(
      (p) =>
        p.display_name.toLowerCase().includes(q) ||
        p.legal_name.toLowerCase().includes(q) ||
        p.tax_number.toLowerCase().includes(q),
    )
    .slice(0, maxMatches);
  return { matches, shouldShowDropdown: true };
}
