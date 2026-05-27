// PR-68 / session-90 — pure-module helpers for the SPA's list-view
// keyboard navigation Tier-1 UX lift. Three operator-facing
// affordances:
//
//   1. `/` focuses the primary search/filter input on a list view
//      (InvoiceList, Partners). Prevents the literal "/" landing in
//      the document; the focus side effect happens in the Svelte
//      wiring layer, NOT here.
//   2. `Esc` clears focus + any filter contents when the focused
//      element is a search input. Existing modal-close behaviour
//      wins on top of this — the Svelte wiring layer ignores the
//      hotkey when the active element isn't *this view's* search
//      input.
//   3. `j` / `k` walk list rows. `Enter` opens the focused row's
//      detail. `g g` jumps to top; `G` to bottom. All four require
//      that no input has focus, so a stray `j` in a textbox stays a
//      literal `j`.
//
// Per the session-90 brief the closed-vocab discriminated union
// (`Hotkey`) is the boundary that makes this extensible without
// stuffing more if-checks into the Svelte handler: adding a future
// hotkey is "extend the union + the parser", not "weave another
// branch through every list view".
//
// The module is intentionally DOM-free: callers pass structurally-
// typed event/target shapes so vitest can pin every variant in the
// node environment (no jsdom dependency added; CLAUDE.md rule 2 —
// keep the dep tree small).
//
// Pinned by `keyboard-nav.test.ts`.

/** PR-68 / session-90 — closed-vocab discriminated union of every
 * hotkey the list views recognise. A future addition (e.g., `?` for
 * help-overlay toggle, `/` chord with shift for a global search)
 * lifts here as a new variant + a new switch arm in [`parseHotkey`];
 * Svelte wiring then handles the new kind explicitly. CLAUDE.md
 * rule 7 (surface conflicts) — the parser refuses to emit an
 * ambiguous result (returns `null`) rather than guessing. */
export type Hotkey =
  | { kind: "focus-search" }
  | { kind: "row-down" }
  | { kind: "row-up" }
  | { kind: "row-open" }
  | { kind: "row-top" }
  | { kind: "row-bottom" }
  | { kind: "blur-or-clear" }
  | { kind: "toggle-hints" };

/** PR-68 / session-90 — structural shape of a target the parser
 * needs to inspect. Real DOM targets (`HTMLInputElement`,
 * `HTMLTextAreaElement`, etc.) already carry these properties; the
 * structural type lets vitest pass plain objects in the pin tests
 * without a jsdom environment. `tagName` is upper-cased by the
 * browser for HTML elements, but the comparison normalises to
 * upper-case for defence in depth (XHTML / SVG cases pass through
 * lower-case). */
export interface TypingTargetLike {
  tagName?: string;
  isContentEditable?: boolean;
}

/** PR-68 / session-90 — "is this target a place the operator is
 * typing into?" If yes, the j/k/Enter/`/` hotkeys MUST stand down
 * so a stray keypress in a textbox is treated as literal text input.
 *
 * Closed vocab: INPUT, TEXTAREA, SELECT, contentEditable.
 *
 *   - INPUT — every text/search/email/number/etc. input.
 *   - TEXTAREA — multi-line text input.
 *   - SELECT — the operator's keypresses drive native option
 *     filtering; we MUST NOT shadow that with our list-row nav.
 *   - contentEditable — defensive (no current ABERP UI uses it,
 *     but a future rich-text field would otherwise silently lose
 *     keystrokes to the list nav).
 *
 * Buttons, links, table cells, the document body — none of those
 * are "typing", so the parser is free to emit a hotkey. */
export function isTypingTarget(target: EventTarget | null): boolean {
  if (target === null) return false;
  const t = target as TypingTargetLike;
  const rawTag = typeof t.tagName === "string" ? t.tagName : "";
  const tag = rawTag.toUpperCase();
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") {
    return true;
  }
  if (t.isContentEditable === true) return true;
  return false;
}

/** PR-68 / session-90 — pure arithmetic for "what row should be
 * focused next?" Bounds-clamped (no wrap-around): pressing `j` at
 * the bottom row stays on the bottom row, `k` at the top stays at
 * the top. Wrap-around feels surprising on a long list — the
 * operator's mental model is a scrolling table, not a circular
 * carousel.
 *
 *   - `total <= 0` → -1 (no row to focus; list is empty)
 *   - `current < 0` (no row focused yet) → first row regardless of
 *     direction. Vim has no "no row" state because the cursor is
 *     always somewhere; we mimic that by parking the first j/k
 *     press at row 0 so the operator's next keypress moves with
 *     known origin.
 *   - Otherwise clamp `current + direction` to `[0, total - 1]`.
 *
 * Pinned by `nextRowIndex_*` cases — bounds, clamp, empty list,
 * negative origin. */
export function nextRowIndex(
  current: number,
  direction: 1 | -1,
  total: number,
): number {
  if (total <= 0) return -1;
  if (current < 0) return 0;
  const next = current + direction;
  if (next < 0) return 0;
  if (next >= total) return total - 1;
  return next;
}

/** PR-68 / session-90 — per-handler state for the "g g" chord. A
 * single press arms `lastGAt`; a second press within
 * [`G_G_WINDOW_MS`] emits `row-top`. Any other key (incl. another
 * `g` outside the window) resets the state.
 *
 * Why a tiny state machine instead of capturing `keydown`-twice
 * upstream: the only chord ABERP currently models is `g g`. A
 * full keymap engine would be over-built (CLAUDE.md rule 2). If a
 * second chord ever lands, lift this to a `Map<string, number>`
 * keyed by sequence prefix. */
export interface HotkeyParserState {
  lastGAt: number | null;
}

export function makeHotkeyParserState(): HotkeyParserState {
  return { lastGAt: null };
}

/** PR-68 / session-90 — `g g` chord timing window. Half a second is
 * the vim-default for `gg`; faster than that lets the operator
 * chord comfortably, slower would let a `g`-then-something-else
 * (`g` + later `g` after a pause) false-trigger row-top. */
export const G_G_WINDOW_MS = 500;

/** PR-68 / session-90 — structural shape of a KeyboardEvent the
 * parser needs. Mirrors the typing-target trick: real DOM events
 * conform; vitest pins synthesise plain objects. */
export interface HotkeyEventLike {
  key: string;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
  target: EventTarget | null;
}

/** PR-68 / session-90 — translate a keydown event into a closed-vocab
 * Hotkey, or null if the event should be ignored at the list-view
 * layer. The function mutates `state` for the `g g` chord (the only
 * stateful case); every other variant is stateless.
 *
 * The `now` parameter is injected so the pin tests can drive the
 * `g g` timing window deterministically; production callers omit
 * it and the default `Date.now()` is used.
 *
 * Return-null discipline (CLAUDE.md rule 12: fail loud, not silent):
 *
 *   - Any ctrl/meta/alt modifier → null. Those modifiers are
 *     reserved for OS / browser shortcuts (Cmd-F, Ctrl-R, etc.); we
 *     never shadow them. Shift is allowed because `G` (shift+g) is
 *     a modeled hotkey.
 *   - Escape inside a typing target → blur-or-clear.
 *   - Escape outside a typing target → null. The list view has
 *     nothing to clear; modal-close handlers manage their own Esc.
 *   - Any other hotkey emitted only when no typing target is
 *     focused. A `j` inside a search box stays a literal `j`.
 *   - Unknown key → null (with the `g g` chord state reset so a
 *     bare `g` followed by an unrelated key doesn't latch). */
export function parseHotkey(
  event: HotkeyEventLike,
  state: HotkeyParserState,
  now: number = Date.now(),
): Hotkey | null {
  if (event.ctrlKey === true || event.metaKey === true || event.altKey === true) {
    state.lastGAt = null;
    return null;
  }

  const typing = isTypingTarget(event.target);

  if (event.key === "Escape") {
    state.lastGAt = null;
    return typing ? { kind: "blur-or-clear" } : null;
  }

  if (typing) {
    state.lastGAt = null;
    return null;
  }

  switch (event.key) {
    case "/": {
      if (event.shiftKey === true) return null;
      state.lastGAt = null;
      return { kind: "focus-search" };
    }
    case "?": {
      // Shift+/ on most keyboards renders `event.key === "?"` with
      // shiftKey true. The hint toggle is intentionally on `?` (vim
      // help convention) so it doesn't compete with the search-focus
      // hotkey on `/`.
      state.lastGAt = null;
      return { kind: "toggle-hints" };
    }
    case "j": {
      if (event.shiftKey === true) return null;
      state.lastGAt = null;
      return { kind: "row-down" };
    }
    case "k": {
      if (event.shiftKey === true) return null;
      state.lastGAt = null;
      return { kind: "row-up" };
    }
    case "Enter": {
      if (event.shiftKey === true) return null;
      state.lastGAt = null;
      return { kind: "row-open" };
    }
    case "g": {
      if (event.shiftKey === true) return null;
      if (state.lastGAt !== null && now - state.lastGAt <= G_G_WINDOW_MS) {
        state.lastGAt = null;
        return { kind: "row-top" };
      }
      state.lastGAt = now;
      return null;
    }
    case "G": {
      if (event.shiftKey !== true) return null;
      state.lastGAt = null;
      return { kind: "row-bottom" };
    }
    default: {
      state.lastGAt = null;
      return null;
    }
  }
}

/** PR-68 / session-90 — case-insensitive substring filter for the
 * InvoiceList screen's `/`-targeted search box. Searches across the
 * three operator-named-load-bearing fields the brief calls out:
 * invoice number (composed `YYYY-NNNNNN`), the raw ULID
 * `invoice_id` (operators occasionally paste it from logs), buyer
 * name (PR-65), and state. Empty / whitespace-only needle returns
 * the full list unchanged (mirrors `filterPartners`'s posture).
 *
 * The Partner-column behaviour mirrors `buyerColumnDisplay`: a null
 * / blank `buyer_name` does NOT match the needle (the operator
 * cannot search for "no value"; the state filter dropdown is the
 * affordance for that). */
export interface InvoiceSearchRow {
  invoice_id: string;
  sequence_number: number;
  fiscal_year: number;
  state: string;
  buyer_name: string | null;
}

export function filterInvoicesByNeedle<R extends InvoiceSearchRow>(
  rows: R[],
  needle: string,
): R[] {
  const q = needle.trim().toLowerCase();
  if (q.length === 0) return rows;
  return rows.filter((r) => {
    const composedNumber = `${r.fiscal_year}-${String(r.sequence_number).padStart(6, "0")}`;
    if (composedNumber.toLowerCase().includes(q)) return true;
    if (r.invoice_id.toLowerCase().includes(q)) return true;
    if (r.state.toLowerCase().includes(q)) return true;
    if (r.buyer_name !== null && r.buyer_name.toLowerCase().includes(q)) {
      return true;
    }
    return false;
  });
}
