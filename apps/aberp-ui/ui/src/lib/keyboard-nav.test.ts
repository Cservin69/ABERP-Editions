// PR-68 / session-90 — pin tests for the SPA's list-view keyboard
// navigation. The pure module (`keyboard-nav.ts`) is the closed-vocab
// boundary; every variant of the `Hotkey` union has its own pin
// + each rejection arm has its own pin, so a regression that
// collapses the parser to a constant cannot pass every assertion
// vacuously (CLAUDE.md rule 9).
//
// The tests run in vitest's default node environment — no jsdom
// dependency is added. `parseHotkey` and `isTypingTarget` accept
// structurally-typed shapes so synthetic objects exercise both.

import { describe, expect, it } from "vitest";

import {
  G_G_WINDOW_MS,
  filterInvoicesByNeedle,
  isTypingTarget,
  makeHotkeyParserState,
  nextRowIndex,
  parseHotkey,
  type HotkeyEventLike,
} from "./keyboard-nav";

// ── isTypingTarget ──────────────────────────────────────────────

describe("isTypingTarget", () => {
  it("returns true for an INPUT element", () => {
    expect(isTypingTarget({ tagName: "INPUT" } as unknown as EventTarget)).toBe(true);
  });

  it("returns true for a TEXTAREA element", () => {
    expect(isTypingTarget({ tagName: "TEXTAREA" } as unknown as EventTarget)).toBe(true);
  });

  it("returns true for a SELECT element (native option filtering must not be shadowed)", () => {
    expect(isTypingTarget({ tagName: "SELECT" } as unknown as EventTarget)).toBe(true);
  });

  it("returns true for a contentEditable element", () => {
    expect(
      isTypingTarget({ tagName: "DIV", isContentEditable: true } as unknown as EventTarget),
    ).toBe(true);
  });

  it("returns false for a BUTTON element (clickable, not typeable)", () => {
    expect(isTypingTarget({ tagName: "BUTTON" } as unknown as EventTarget)).toBe(false);
  });

  it("returns false for the document body (no element actually focused)", () => {
    expect(isTypingTarget({ tagName: "BODY" } as unknown as EventTarget)).toBe(false);
  });

  it("returns false for a null target (no focus)", () => {
    expect(isTypingTarget(null)).toBe(false);
  });

  it("normalises lower-case tag names (SVG / XHTML edge case)", () => {
    expect(isTypingTarget({ tagName: "input" } as unknown as EventTarget)).toBe(true);
  });
});

// ── nextRowIndex ────────────────────────────────────────────────

describe("nextRowIndex", () => {
  it("returns -1 when the list is empty", () => {
    expect(nextRowIndex(-1, 1, 0)).toBe(-1);
    expect(nextRowIndex(0, -1, 0)).toBe(-1);
  });

  it("parks the first j/k press at row 0 when no row is focused", () => {
    expect(nextRowIndex(-1, 1, 5)).toBe(0);
    expect(nextRowIndex(-1, -1, 5)).toBe(0);
  });

  it("moves forward by one inside the list", () => {
    expect(nextRowIndex(0, 1, 5)).toBe(1);
    expect(nextRowIndex(3, 1, 5)).toBe(4);
  });

  it("moves backward by one inside the list", () => {
    expect(nextRowIndex(4, -1, 5)).toBe(3);
    expect(nextRowIndex(1, -1, 5)).toBe(0);
  });

  it("clamps at the top instead of wrapping", () => {
    expect(nextRowIndex(0, -1, 5)).toBe(0);
  });

  it("clamps at the bottom instead of wrapping", () => {
    expect(nextRowIndex(4, 1, 5)).toBe(4);
  });

  it("treats a single-row list correctly", () => {
    expect(nextRowIndex(0, 1, 1)).toBe(0);
    expect(nextRowIndex(0, -1, 1)).toBe(0);
    expect(nextRowIndex(-1, 1, 1)).toBe(0);
  });
});

// ── parseHotkey: per-variant pins ────────────────────────────────

function mkEvent(over: Partial<HotkeyEventLike> & { key: string }): HotkeyEventLike {
  return {
    key: over.key,
    ctrlKey: over.ctrlKey ?? false,
    metaKey: over.metaKey ?? false,
    altKey: over.altKey ?? false,
    shiftKey: over.shiftKey ?? false,
    target: over.target ?? ({ tagName: "BODY" } as unknown as EventTarget),
  };
}

describe("parseHotkey — variants", () => {
  it("`/` → focus-search (no typing target)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "/" }), state)).toEqual({ kind: "focus-search" });
  });

  it("`j` → row-down (no typing target)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "j" }), state)).toEqual({ kind: "row-down" });
  });

  it("`k` → row-up (no typing target)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "k" }), state)).toEqual({ kind: "row-up" });
  });

  it("`Enter` → row-open (no typing target)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "Enter" }), state)).toEqual({ kind: "row-open" });
  });

  it("`G` (shift+g) → row-bottom", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "G", shiftKey: true }), state)).toEqual({
      kind: "row-bottom",
    });
  });

  it("`?` (shift+/) → toggle-hints", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "?", shiftKey: true }), state)).toEqual({
      kind: "toggle-hints",
    });
  });

  it("`Escape` inside a search input → blur-or-clear", () => {
    const state = makeHotkeyParserState();
    const input = { tagName: "INPUT" } as unknown as EventTarget;
    expect(parseHotkey(mkEvent({ key: "Escape", target: input }), state)).toEqual({
      kind: "blur-or-clear",
    });
  });
});

// ── parseHotkey: rejection arms ──────────────────────────────────

describe("parseHotkey — rejection arms", () => {
  it("returns null when ctrl is held (OS shortcut zone)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "j", ctrlKey: true }), state)).toBeNull();
    expect(parseHotkey(mkEvent({ key: "/", ctrlKey: true }), state)).toBeNull();
  });

  it("returns null when meta (cmd) is held (OS shortcut zone)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "j", metaKey: true }), state)).toBeNull();
  });

  it("returns null when alt is held (OS shortcut zone)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "j", altKey: true }), state)).toBeNull();
  });

  it("returns null for j/k/`/`/Enter when an input is focused", () => {
    const state = makeHotkeyParserState();
    const input = { tagName: "INPUT" } as unknown as EventTarget;
    expect(parseHotkey(mkEvent({ key: "j", target: input }), state)).toBeNull();
    expect(parseHotkey(mkEvent({ key: "k", target: input }), state)).toBeNull();
    expect(parseHotkey(mkEvent({ key: "/", target: input }), state)).toBeNull();
    expect(parseHotkey(mkEvent({ key: "Enter", target: input }), state)).toBeNull();
  });

  it("returns null for Escape outside an input (modal handlers manage their own Esc)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "Escape" }), state)).toBeNull();
  });

  it("returns null for an unmodeled key", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "x" }), state)).toBeNull();
    expect(parseHotkey(mkEvent({ key: "Tab" }), state)).toBeNull();
  });

  it("returns null for capital `J` (shift modifier not modeled for j/k)", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "J", shiftKey: true }), state)).toBeNull();
  });

  it("returns null for lower-case `g` (without shift) once — first half of the chord", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "g" }), state, 1000)).toBeNull();
  });
});

// ── parseHotkey: `g g` chord state machine ──────────────────────

describe("parseHotkey — `g g` chord", () => {
  it("first `g` returns null and arms the state; second `g` within window → row-top", () => {
    const state = makeHotkeyParserState();
    expect(parseHotkey(mkEvent({ key: "g" }), state, 1000)).toBeNull();
    expect(parseHotkey(mkEvent({ key: "g" }), state, 1000 + 200)).toEqual({ kind: "row-top" });
  });

  it("third consecutive `g` re-arms (each chord is independent)", () => {
    const state = makeHotkeyParserState();
    parseHotkey(mkEvent({ key: "g" }), state, 1000);
    parseHotkey(mkEvent({ key: "g" }), state, 1100); // emits row-top, resets state
    expect(parseHotkey(mkEvent({ key: "g" }), state, 1200)).toBeNull();
    expect(parseHotkey(mkEvent({ key: "g" }), state, 1300)).toEqual({ kind: "row-top" });
  });

  it("second `g` outside the window does NOT fire row-top — it re-arms", () => {
    const state = makeHotkeyParserState();
    parseHotkey(mkEvent({ key: "g" }), state, 1000);
    expect(
      parseHotkey(mkEvent({ key: "g" }), state, 1000 + G_G_WINDOW_MS + 1),
    ).toBeNull();
    // The late second `g` re-armed; another within window completes
    // the chord.
    expect(parseHotkey(mkEvent({ key: "g" }), state, 1000 + G_G_WINDOW_MS + 200)).toEqual(
      { kind: "row-top" },
    );
  });

  it("any non-`g` key between the two presses resets the chord", () => {
    const state = makeHotkeyParserState();
    parseHotkey(mkEvent({ key: "g" }), state, 1000);
    // A stray `j` between the two `g`s resets the chord.
    parseHotkey(mkEvent({ key: "j" }), state, 1050);
    expect(parseHotkey(mkEvent({ key: "g" }), state, 1100)).toBeNull();
  });

  it("Shift+G is row-bottom (separate hotkey), does NOT complete the chord", () => {
    const state = makeHotkeyParserState();
    parseHotkey(mkEvent({ key: "g" }), state, 1000);
    expect(parseHotkey(mkEvent({ key: "G", shiftKey: true }), state, 1100)).toEqual({
      kind: "row-bottom",
    });
  });
});

// ── filterInvoicesByNeedle ──────────────────────────────────────

interface RowFixture {
  invoice_id: string;
  sequence_number: number;
  fiscal_year: number;
  state: string;
  buyer_name: string | null;
}

const ROWS: RowFixture[] = [
  {
    invoice_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
    sequence_number: 13,
    fiscal_year: 2026,
    state: "Finalized",
    buyer_name: "Budapesti Sport-Egyesület Kft.",
  },
  {
    invoice_id: "01ARZ3NDEKTSV4RRFFQ69G5FBX",
    sequence_number: 14,
    fiscal_year: 2026,
    state: "Ready",
    buyer_name: "Magyar Telekom Nyrt.",
  },
  {
    invoice_id: "01ARZ3NDEKTSV4RRFFQ69G5FCY",
    sequence_number: 1,
    fiscal_year: 2026,
    state: "Storno",
    buyer_name: null,
  },
];

describe("filterInvoicesByNeedle", () => {
  it("returns every row when the needle is empty", () => {
    expect(filterInvoicesByNeedle(ROWS, "")).toEqual(ROWS);
    expect(filterInvoicesByNeedle(ROWS, "   ")).toEqual(ROWS);
  });

  it("matches the composed invoice number (YYYY-NNNNNN)", () => {
    const matches = filterInvoicesByNeedle(ROWS, "2026-000013");
    expect(matches.map((r) => r.sequence_number)).toEqual([13]);
  });

  it("matches a partial composed-number prefix (operators often type the year + start)", () => {
    const matches = filterInvoicesByNeedle(ROWS, "000014");
    expect(matches.map((r) => r.sequence_number)).toEqual([14]);
  });

  it("matches the ULID invoice_id", () => {
    // The ULID is the unique key on the wire; surfacing it via search
    // lets operators paste an id from a log into the filter box.
    const matches = filterInvoicesByNeedle(ROWS, "5FBX");
    expect(matches.map((r) => r.sequence_number)).toEqual([14]);
  });

  it("matches the buyer name case-insensitively", () => {
    const matches = filterInvoicesByNeedle(ROWS, "telekom");
    expect(matches.map((r) => r.sequence_number)).toEqual([14]);
  });

  it("matches the state label", () => {
    const matches = filterInvoicesByNeedle(ROWS, "Storno");
    expect(matches.map((r) => r.sequence_number)).toEqual([1]);
  });

  it("returns empty when no row matches", () => {
    expect(filterInvoicesByNeedle(ROWS, "no-such-text")).toEqual([]);
  });

  it("a null buyer_name does NOT match (operators can't search for absence)", () => {
    // The Storno row's buyer_name is null. A needle that overlaps the
    // em-dash placeholder MUST NOT match — the dropdown filter is the
    // affordance for "no value" filtering.
    expect(filterInvoicesByNeedle(ROWS, "—")).toEqual([]);
  });
});
