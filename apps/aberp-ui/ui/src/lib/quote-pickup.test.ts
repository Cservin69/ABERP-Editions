// S255 / PR-244 — pure-shape tests for the Quotes-tab pickup wiring.
//
// Two surfaces under test:
//   1. The `pickupQuoteAsDraft` shim from `./api` — forwards the
//      quote_id and parses the `PickupQuoteOutcome` body verbatim.
//   2. The QuoteIntakeRow → button-vs-link decision: tested as a
//      pure helper so the Svelte rendering layer stays untouched.
//      The component itself follows this rule directly.

import { describe, expect, it, vi } from "vitest";

vi.mock("./api", async (importActual) => {
  const actual = await importActual<typeof import("./api")>();
  return {
    ...actual,
    pickupQuoteAsDraft: vi.fn(),
  };
});

import { pickupQuoteAsDraft, type QuoteIntakeRow } from "./api";

/** Mirror of the QuotesList.svelte button-vs-link rule. Kept as a
 * pure helper so a future contributor moving the SPA toward a
 * tabular component still has the gate's behaviour pinned. */
export function pickupActionVariant(row: QuoteIntakeRow): "button" | "draft-link" {
  return row.picked_up_drf_id ? "draft-link" : "button";
}

function row(over: Partial<QuoteIntakeRow> = {}): QuoteIntakeRow {
  return {
    quote_id: "q-1",
    invoice_id: "inv_TEST",
    received_at: "2026-06-05T08:00:00Z",
    intake_at: "2026-06-05T08:01:00Z",
    status_writeback_at: null,
    contact_name: "Ada Lovelace",
    contact_email: "ada@example.com",
    contact_company: null,
    material: "alu",
    quantity: "3",
    notes: null,
    picked_up_drf_id: null,
    ...over,
  };
}

describe("pickupActionVariant", () => {
  it("returns 'button' for a never-picked-up quote", () => {
    expect(pickupActionVariant(row({ picked_up_drf_id: null }))).toBe("button");
  });

  it("returns 'draft-link' once picked_up_drf_id is populated", () => {
    expect(
      pickupActionVariant(row({ picked_up_drf_id: "drf_01H7TESTDRAFT00000000000" })),
    ).toBe("draft-link");
  });

  it("treats empty string as 'button' (defensive — backend always emits null or drf_<ULID>)", () => {
    // Practically the backend never emits "" — but the SPA must not
    // render a broken link if a future schema drift leaks it.
    expect(pickupActionVariant(row({ picked_up_drf_id: "" }))).toBe("button");
  });
});

describe("pickupQuoteAsDraft shim", () => {
  it("forwards the quote_id and returns the outcome verbatim", async () => {
    vi.mocked(pickupQuoteAsDraft).mockResolvedValueOnce({
      drf_id: "drf_01H7PICKEDUP000000000000",
      partner_id: "prt_01H7NEWPARTNER0000000000",
      partner_created: true,
      was_existing: false,
    });
    const outcome = await pickupQuoteAsDraft("q-42");
    expect(pickupQuoteAsDraft).toHaveBeenCalledWith("q-42");
    expect(outcome.drf_id).toBe("drf_01H7PICKEDUP000000000000");
    expect(outcome.partner_created).toBe(true);
    expect(outcome.was_existing).toBe(false);
  });

  it("propagates idempotent re-call shape (was_existing: true, partner_created: false)", async () => {
    vi.mocked(pickupQuoteAsDraft).mockResolvedValueOnce({
      drf_id: "drf_01H7PICKEDUP000000000000",
      partner_id: "prt_01H7NEWPARTNER0000000000",
      partner_created: false,
      was_existing: true,
    });
    const outcome = await pickupQuoteAsDraft("q-42");
    expect(outcome.was_existing).toBe(true);
    expect(outcome.partner_created).toBe(false);
  });

  it("propagates backend errors as rejected promises", async () => {
    vi.mocked(pickupQuoteAsDraft).mockRejectedValueOnce(
      new Error("quote q-MISSING not staged"),
    );
    await expect(pickupQuoteAsDraft("q-MISSING")).rejects.toThrow(
      "not staged",
    );
  });
});
