// PR-172 — vitest pins for the NotesAutocomplete filter logic.
// Companion to `notes-autocomplete.ts` and the NotesAutocomplete.svelte
// shell. The Svelte renderer itself is unwrapped by vitest (no
// jsdom layer is configured); the load-bearing decisions are in
// `filterNotesByPrefix` and pinned here.

import { describe, expect, it } from "vitest";

import { filterNotesByPrefix } from "./notes-autocomplete";

describe("filterNotesByPrefix", () => {
  const history = [
    "Köszönjük együttműködését",
    "Garancia: 1 év",
    "Helyszíni átadás",
    "Áthozat előző számláról",
    "Köszönjük megrendelését",
  ];

  it("returns the first topN entries verbatim when input is empty", () => {
    expect(filterNotesByPrefix(history, "", 3)).toEqual([
      "Köszönjük együttműködését",
      "Garancia: 1 év",
      "Helyszíni átadás",
    ]);
  });

  it("treats whitespace-only input as empty (no silent miss)", () => {
    expect(filterNotesByPrefix(history, "   ", 2)).toEqual([
      "Köszönjük együttműködését",
      "Garancia: 1 év",
    ]);
  });

  it("matches case-insensitively on startsWith only", () => {
    expect(filterNotesByPrefix(history, "köszön", 5)).toEqual([
      "Köszönjük együttműködését",
      "Köszönjük megrendelését",
    ]);
  });

  it("does NOT match mid-string substrings (predictability over fuzz)", () => {
    expect(filterNotesByPrefix(history, "együttműködését", 5)).toEqual([]);
  });

  it("respects topN as a hard cap", () => {
    expect(filterNotesByPrefix(history, "k", 1)).toEqual([
      "Köszönjük együttműködését",
    ]);
  });

  it("returns an empty list when topN is zero", () => {
    expect(filterNotesByPrefix(history, "k", 0)).toEqual([]);
  });

  it("skips an exact-match echo (suggesting back what was typed is noise)", () => {
    expect(
      filterNotesByPrefix(history, "Köszönjük együttműködését", 5),
    ).toEqual([]);
  });

  it("preserves the original casing of surfaced entries (does not lowercase output)", () => {
    const out = filterNotesByPrefix(history, "g", 1);
    expect(out).toEqual(["Garancia: 1 év"]);
  });

  it("returns an empty list when no history is available", () => {
    expect(filterNotesByPrefix([], "anything", 10)).toEqual([]);
  });
});
