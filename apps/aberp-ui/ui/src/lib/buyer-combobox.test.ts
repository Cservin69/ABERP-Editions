// PR-74 / session-96 — vitest pins for the buyer combobox state helper.
//
// Covers the brief's named failure modes:
//   - Happy path: a 3+ char needle that matches a saved partner
//     surfaces it in the dropdown with `shouldShowDropdown = true`.
//   - No-results recovery: a 3+ char needle that matches nothing
//     surfaces an empty `matches` array with `shouldShowDropdown =
//     true` (the renderer surfaces "no match" affordance; the input
//     value flows through as a one-off buyer name on submit).
//   - Pick vs type-through: the helper does not autofill anything
//     — that's the renderer's job. The helper just tells the
//     renderer what to show.

import { describe, expect, it } from "vitest";

import { buyerComboboxState } from "./buyer-combobox";
import type { Partner } from "./api";

function partner(overrides: Partial<Partner>): Partner {
  return {
    id: "prt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    display_name: "Example Kft.",
    legal_name: "Example Kereskedelmi Kft.",
    kind: "Customer",
    // PR-97 / ADR-0048 — preserve pre-PR-97 implicit Domestic posture
    // for legacy buyer-combobox fixtures.
    customer_vat_status: "Domestic",
    customer_type: "unset",
    tax_number: "12345678-2-13",
    eu_vat_number: null,
    address_street: null,
    address_postal_code: null,
    address_city: null,
    address_country: null,
    bank_account: null,
    contact_email: null,
    contact_phone: null,
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    deleted_at: null,
    ...overrides,
  };
}

describe("buyerComboboxState — happy path (typeahead returns results)", () => {
  it("returns the matching saved partner once needle reaches 3+ chars", () => {
    const saved: Partner[] = [
      partner({ id: "prt_a", display_name: "Acme Kft.", legal_name: "Acme Kereskedelmi Kft.", tax_number: "11111111-2-11" }),
      partner({ id: "prt_b", display_name: "BSCE", legal_name: "Budapesti Sport-Egyesület", tax_number: "22222222-2-22" }),
      partner({ id: "prt_c", display_name: "Csomag Zrt.", legal_name: "Csomag Zártkörűen Működő Rt.", tax_number: "33333333-2-33" }),
    ];

    const result = buyerComboboxState({ needle: "BSC", savedPartners: saved });

    expect(result.shouldShowDropdown).toBe(true);
    expect(result.matches.map((p) => p.id)).toEqual(["prt_b"]);
  });

  it("matches case-insensitively across display_name, legal_name, and tax_number", () => {
    const saved: Partner[] = [
      partner({ id: "prt_a", display_name: "Acme Kft.", legal_name: "ACME Trading", tax_number: "98765432-2-13" }),
      partner({ id: "prt_b", display_name: "Bocci Kft.", legal_name: "Bocci Holdings", tax_number: "11112222-2-13" }),
    ];

    // Substring on display_name (lowercase needle, mixed-case haystack).
    expect(buyerComboboxState({ needle: "acm", savedPartners: saved }).matches.map((p) => p.id)).toEqual(["prt_a"]);
    // Substring on legal_name.
    expect(buyerComboboxState({ needle: "TRADING", savedPartners: saved }).matches.map((p) => p.id)).toEqual(["prt_a"]);
    // Substring on tax_number (a digit prefix the operator recalls).
    expect(buyerComboboxState({ needle: "98765432", savedPartners: saved }).matches.map((p) => p.id)).toEqual(["prt_a"]);
  });

  it("caps the dropdown at maxMatches (default 8)", () => {
    const saved: Partner[] = Array.from({ length: 20 }).map((_, i) =>
      partner({ id: `prt_${i}`, display_name: `Match-${i}`, legal_name: "Match Inc.", tax_number: `0000000${i}-2-13` }),
    );

    const result = buyerComboboxState({ needle: "Match", savedPartners: saved });

    expect(result.matches.length).toBe(8);
  });

  it("respects an explicit maxMatches override", () => {
    const saved: Partner[] = Array.from({ length: 20 }).map((_, i) =>
      partner({ id: `prt_${i}`, display_name: `Match-${i}`, legal_name: "Match Inc.", tax_number: `0000000${i}-2-13` }),
    );

    const result = buyerComboboxState({ needle: "Match", savedPartners: saved, maxMatches: 3 });

    expect(result.matches.length).toBe(3);
  });
});

describe("buyerComboboxState — no-results recovery", () => {
  it("returns empty matches with shouldShowDropdown=true when nothing matches", () => {
    // The renderer surfaces this as a 'no match' hint; the input
    // value flows through as a one-off buyer name on submit. The
    // dropdown stays visible so the operator can see "we tried and
    // found nothing" — silently hiding it was the PR-54 footgun
    // PR-74 closes.
    const saved: Partner[] = [
      partner({ id: "prt_a", display_name: "Acme Kft.", legal_name: "Acme Trading", tax_number: "11111111-2-11" }),
    ];

    const result = buyerComboboxState({ needle: "Zebra Holdings", savedPartners: saved });

    expect(result.shouldShowDropdown).toBe(true);
    expect(result.matches).toEqual([]);
  });
});

describe("buyerComboboxState — gating below minChars", () => {
  it("does not show the dropdown until the trimmed needle reaches minChars", () => {
    const saved: Partner[] = [partner({ id: "prt_a", display_name: "Acme Kft." })];

    expect(buyerComboboxState({ needle: "", savedPartners: saved }).shouldShowDropdown).toBe(false);
    expect(buyerComboboxState({ needle: "ac", savedPartners: saved }).shouldShowDropdown).toBe(false);
    // The leading + trailing whitespace must not count toward the
    // threshold — `"  ab  "` is still under 3 real chars.
    expect(buyerComboboxState({ needle: "  ab  ", savedPartners: saved }).shouldShowDropdown).toBe(false);
    expect(buyerComboboxState({ needle: "acm", savedPartners: saved }).shouldShowDropdown).toBe(true);
  });

  it("returns empty matches when below minChars even if savedPartners is non-empty", () => {
    const saved: Partner[] = [partner({ id: "prt_a", display_name: "Acme Kft." })];

    const result = buyerComboboxState({ needle: "a", savedPartners: saved });

    expect(result.matches).toEqual([]);
    expect(result.shouldShowDropdown).toBe(false);
  });

  it("respects an explicit minChars override (operator opts into instant search)", () => {
    const saved: Partner[] = [partner({ id: "prt_a", display_name: "Acme Kft." })];

    const result = buyerComboboxState({ needle: "a", savedPartners: saved, minChars: 1 });

    expect(result.shouldShowDropdown).toBe(true);
    expect(result.matches.map((p) => p.id)).toEqual(["prt_a"]);
  });
});

describe("buyerComboboxState — empty saved-partners list", () => {
  it("surfaces shouldShowDropdown=true even when savedPartners is empty", () => {
    // First-time tenant scenario: no partners saved yet. The operator
    // typing in the buyer-name input must still see the 'no match' hint
    // (rather than the dropdown silently never showing — which would
    // make them wonder if the feature is broken).
    const result = buyerComboboxState({ needle: "Brand New Kft.", savedPartners: [] });

    expect(result.shouldShowDropdown).toBe(true);
    expect(result.matches).toEqual([]);
  });
});

describe("buyerComboboxState — live wire-shape pin (PR-75 / session-99)", () => {
  it("matches against the exact snake_case JSON the GET /api/partners route emits", () => {
    // PR-75 / session-99 — pin against the verbatim shape
    // `aberp::partners::Partner` (the Rust serde target with NO
    // `rename_all` directive) emits on the wire. A future drift
    // (renaming `display_name` → `name`, or changing snake_case to
    // camelCase) would silently break the live combobox without this
    // test catching it. Construct the fixture by hand rather than via
    // the partner() factory above so the field names + types are
    // visually verifiable against the Rust struct in
    // `apps/aberp/src/partners.rs::Partner`.
    const wireShape: Partner = {
      id: "prt_01HXYZABCDEFGHJKMNPQRSTVWX",
      display_name: "BSCE",
      legal_name: "Budapesti Sport-Egyesület Kft.",
      kind: "Customer",
      // PR-97 / ADR-0048 — wire-shape pin includes the new field.
      customer_vat_status: "Domestic",
      customer_type: "unset",
      tax_number: "22222222-2-22",
      eu_vat_number: "HU22222222",
      address_street: "Üllői út 1.",
      address_postal_code: "1097",
      address_city: "Budapest",
      address_country: "Magyarország",
      bank_account: "12345678-12345678-12345678",
      contact_email: "billing@bsce.example",
      contact_phone: "+36 1 234 5678",
      created_at: "2026-05-01T08:00:00Z",
      updated_at: "2026-05-20T15:30:00Z",
      deleted_at: null,
    };

    // Match by display_name (the short label the operator typically
    // recalls).
    const byDisplay = buyerComboboxState({
      needle: "BSCE",
      savedPartners: [wireShape],
    });
    expect(byDisplay.shouldShowDropdown).toBe(true);
    expect(byDisplay.matches).toEqual([wireShape]);

    // Match by legal_name substring (case-insensitive Hungarian).
    const byLegal = buyerComboboxState({
      needle: "egyesület",
      savedPartners: [wireShape],
    });
    expect(byLegal.matches).toEqual([wireShape]);

    // Match by tax_number prefix the operator might recall.
    const byTax = buyerComboboxState({
      needle: "22222222",
      savedPartners: [wireShape],
    });
    expect(byTax.matches).toEqual([wireShape]);
  });
});
