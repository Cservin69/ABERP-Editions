// PR-65 / session-86 — pure-module helpers for the SPA's
// list-row Tier-1 UX lift. Extends the composer-pin pattern
// (A156 / A161 / A163) so vitest pins the per-row buyer-label
// fallback + the quick-action filter table without mounting a
// Svelte 5 component (no `@testing-library/svelte` dep in this
// workspace; the project keeps the component-test runner
// name-deferred per CLAUDE.md rule 2 — pure helpers carry
// every load-bearing decision).
//
// Pinned by `invoice-list.test.ts`.

import type { DetailActionButton } from "./invoice-actions";
import { buttonsForState } from "./invoice-actions";
import type { InvoiceState } from "./api";

/** PR-65 / session-86 — quiet em-dash placeholder for the Partner
 * column when the backend has no buyer name to surface (CLI-issued
 * invoice, pre-PR-47α SPA-issued invoice, or a side-store I/O
 * failure). U+2014 (EM DASH) matches the ADR-0017 §1-2 quiet-
 * chrome posture for "no value to render here" cells. Pinned by
 * `buyer_column_display_em_dash_for_null` so a regression that
 * swaps the glyph for `"-"` / `"N/A"` / empty string surfaces at
 * `npm test`. */
export const PARTNER_COLUMN_EM_DASH = "—";

/** PR-65 / session-86 — fold a nullable buyer-name into the string
 * the list row's Partner cell renders. `null` / blank / whitespace-
 * only collapse to the em-dash placeholder so the operator never
 * sees a missing-buyer row as a totally empty cell (ambiguous with
 * "data not yet loaded"). The backend already trims + returns
 * `None` for blank values (`read_buyer_name_from_side_store`), so
 * the blank branch here is defence in depth.
 *
 * Returns a plain string the renderer drops into the cell verbatim
 * — the Svelte renderer applies the muted styling via a CSS class
 * on the `<td>`, NOT by inspecting the returned value, so the
 * em-dash glyph is the only signal both code paths share. */
export function buyerColumnDisplay(name: string | null): string {
  if (name === null) return PARTNER_COLUMN_EM_DASH;
  const trimmed = name.trim();
  return trimmed.length > 0 ? trimmed : PARTNER_COLUMN_EM_DASH;
}

/** PR-65 / session-86 — closed-vocab of per-row quick-action
 * buttons. Subset of the detail modal's [`DetailActionButton`]
 * vocab: only the three operator-named-as-load-bearing buttons
 * (Download / Submit / Storno) make sense as one-click row
 * affordances. PollAck stays detail-modal-only because its
 * bounded 31s poll loop benefits from the modal's larger error
 * surface; Modification stays modal-only because it OPENS a fresh
 * form (the operator edits the corrected body), not a one-click
 * action. Per CLAUDE.md rule 3 (surgical) the narrow vocab is
 * the surface area the brief explicitly named — adding the other
 * two would inherit two new failure modes for no operator gain. */
export type RowQuickAction = Extract<
  DetailActionButton,
  "Download" | "Submit" | "Storno" | "Pay"
>;

/** PR-65 / session-86 — filter the detail-modal button table down
 * to the row-quick-action buttons in operator-reading order.
 * Mirror invariant: the returned subset must preserve the per-state
 * gating of [`buttonsForState`] so a row-level click never bypasses
 * the precondition guard the backend enforces (a Storno button
 * appearing on a `Ready` row would produce a 409 the operator was
 * not warned about — exactly the failure mode A161 / A163 named).
 *
 * PR-70 / ADR-0039 — `paid` second parameter threads through to the
 * upstream `buttonsForState` so the row's Pay quick-action mirrors
 * the modal's Pay button gating. The output order places `Pay` after
 * `Submit` and before `Storno` so the operator-most-common flow
 * (Finalized → record payment) sits at a predictable column index.
 *
 * The output order mirrors the brief's `📄 PDF / ↗ Submit / 💰 Pay /
 * ⊘ Storno` left-to-right placement so the operator's eye finds each
 * glyph at a consistent column index across rows. Pinned by
 * `quick_actions_for_state_table_mirror`. */
export function quickActionsForState(
  state: InvoiceState,
  paid: boolean = false,
): RowQuickAction[] {
  const detail = buttonsForState(state, paid);
  const out: RowQuickAction[] = [];
  // Preserve a stable column order regardless of the detail table's
  // emission order. The detail table's per-state order is the
  // modal-header reading order; the list row's order is the column-
  // index order. Decoupling them keeps each surface's UX intent
  // independent.
  if (detail.includes("Download")) out.push("Download");
  if (detail.includes("Submit")) out.push("Submit");
  if (detail.includes("Pay")) out.push("Pay");
  if (detail.includes("Storno")) out.push("Storno");
  return out;
}

/** PR-65 / session-86 — per-button presentational metadata for the
 * row-quick-action renderer. `glyph` is the leading icon (mirrors
 * the brief's `📄 / ↗ / ⊘`); `label` is the screen-reader + tooltip
 * text. Renderer composes both into the button (visual = glyph,
 * `aria-label` = `${label} invoice ${id}`). Pinned by
 * `quick_action_meta_table_round_trip` so a glyph drift surfaces. */
export interface QuickActionMeta {
  glyph: string;
  label: string;
}

export function quickActionMeta(action: RowQuickAction): QuickActionMeta {
  switch (action) {
    case "Download":
      return { glyph: "📄", label: "Download PDF" };
    case "Submit":
      return { glyph: "↗", label: "Submit to NAV" };
    case "Pay":
      return { glyph: "💰", label: "Mark as paid" };
    case "Storno":
      return { glyph: "⊘", label: "Cancel (storno)" };
  }
}
