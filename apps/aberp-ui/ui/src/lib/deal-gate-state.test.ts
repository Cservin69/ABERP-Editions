// S272 / PR-261 — pin for the EVE addenda 2 (UI half) + 3 contract.
//
// Pure-helper tests rather than a Svelte renderer: the gate's visual
// contract IS the function below. The 3 golden states the brief calls
// out (empty / wrong / correct) each get their own assertion bundle,
// plus the cross-product with `stockAlert` × REFRESH acknowledgement.

import { describe, expect, it } from "vitest";

import {
  REFRESH_ACK_TOKEN,
  dealSagaErrorTitle,
  deriveDealGateState,
  expectedDealToken,
} from "./deal-gate-state";

// Storefront-shaped UUID quote_id; the design doc example.
const QUOTE_ID = "0226e154-9e6c-4c0a-9001-f3a8a0c0a000";
const EXPECTED = "0226e154";

describe("expectedDealToken — DEAL token is the first 8 chars of quote_id", () => {
  it("takes the first 8 characters of a UUID-shaped quote_id verbatim", () => {
    expect(expectedDealToken(QUOTE_ID)).toBe(EXPECTED);
  });

  it("returns the whole id when shorter than 8 chars (defensive — test ids)", () => {
    expect(expectedDealToken("q-1")).toBe("q-1");
    expect(expectedDealToken("12345")).toBe("12345");
    expect(expectedDealToken("12345678")).toBe("12345678");
  });

  it("never extends past 8 chars (cap)", () => {
    expect(expectedDealToken("123456789")).toBe("12345678");
    expect(expectedDealToken("abcdefghij")).toBe("abcdefgh");
  });
});

describe("deriveDealGateState — EVE addendum 3 visual contract", () => {
  // The brief's "visual golden test" — render the DEAL section in 3
  // states and pin the tone strings. The Svelte template binds these
  // directly to `data-tone="..."`, so a tone regression surfaces here
  // first, before reaching prod.

  it("empty input → tone=empty (RED affordance), canSubmit=false", () => {
    const state = deriveDealGateState({
      quoteId: QUOTE_ID,
      stockAlert: false,
      refreshInput: "",
      dealInput: "",
      submitting: false,
    });
    expect(state).toEqual({
      expectedDealToken: EXPECTED,
      refreshAcked: false,
      dealTokenMatches: false,
      dealTone: "empty",
      canSubmit: false,
    });
  });

  it("wrong input → tone=wrong (RED affordance + helper text), canSubmit=false", () => {
    const state = deriveDealGateState({
      quoteId: QUOTE_ID,
      stockAlert: false,
      refreshInput: "",
      dealInput: "00000000",
      submitting: false,
    });
    expect(state.dealTone).toBe("wrong");
    expect(state.canSubmit).toBe(false);
  });

  it("correct input → tone=correct (GREEN confirm), canSubmit=true on stock-OK row", () => {
    const state = deriveDealGateState({
      quoteId: QUOTE_ID,
      stockAlert: false,
      refreshInput: "",
      dealInput: EXPECTED,
      submitting: false,
    });
    expect(state.dealTone).toBe("correct");
    expect(state.canSubmit).toBe(true);
  });

  it("case-sensitive — uppercase variant of the token is REJECTED (per hülye-biztos)", () => {
    const state = deriveDealGateState({
      quoteId: QUOTE_ID,
      stockAlert: false,
      refreshInput: "",
      dealInput: EXPECTED.toUpperCase(),
      submitting: false,
    });
    expect(state.dealTokenMatches).toBe(false);
    expect(state.dealTone).toBe("wrong");
    expect(state.canSubmit).toBe(false);
  });

  it("submitting=true blocks canSubmit even when the token matches", () => {
    const state = deriveDealGateState({
      quoteId: QUOTE_ID,
      stockAlert: false,
      refreshInput: "",
      dealInput: EXPECTED,
      submitting: true,
    });
    expect(state.canSubmit).toBe(false);
  });
});

describe("deriveDealGateState × REFRESH gate (EVE addendum 2 UI half)", () => {
  it("stockAlert=true + REFRESH unacked → canSubmit=false even with matching DEAL token", () => {
    const state = deriveDealGateState({
      quoteId: QUOTE_ID,
      stockAlert: true,
      refreshInput: "",
      dealInput: EXPECTED,
      submitting: false,
    });
    expect(state.refreshAcked).toBe(false);
    expect(state.dealTokenMatches).toBe(true);
    expect(state.canSubmit).toBe(false);
  });

  it("stockAlert=true + REFRESH acked + matching DEAL token → canSubmit=true", () => {
    const state = deriveDealGateState({
      quoteId: QUOTE_ID,
      stockAlert: true,
      refreshInput: REFRESH_ACK_TOKEN,
      dealInput: EXPECTED,
      submitting: false,
    });
    expect(state.refreshAcked).toBe(true);
    expect(state.canSubmit).toBe(true);
  });

  it("REFRESH is case-sensitive — 'refresh' / 'Refresh' do NOT ack", () => {
    for (const variant of ["refresh", "Refresh", "REFRESH ", " REFRESH"]) {
      const state = deriveDealGateState({
        quoteId: QUOTE_ID,
        stockAlert: true,
        refreshInput: variant,
        dealInput: EXPECTED,
        submitting: false,
      });
      expect(state.refreshAcked).toBe(false);
      expect(state.canSubmit).toBe(false);
    }
  });

  it("stockAlert=false → refresh input is irrelevant, canSubmit only depends on DEAL token", () => {
    const state = deriveDealGateState({
      quoteId: QUOTE_ID,
      stockAlert: false,
      refreshInput: "garbage",
      dealInput: EXPECTED,
      submitting: false,
    });
    expect(state.canSubmit).toBe(true);
  });
});

describe("dealSagaErrorTitle — 409 machine-code → operator-facing copy", () => {
  // Snapshot each closed-vocab code's bilingual copy. A future PR
  // adding a code will fail the `unknown` fall-through assertion, so
  // the maintainer knows where to add the arm.

  it.each([
    ["stock_alert_refresh_required", "REFRESH kötelező / REFRESH required"],
    ["deal_already_issued", "Ez a DEAL már kiállításra került / DEAL already issued"],
    ["deal_token_mismatch", "Token nem egyezik / Token does not match"],
    ["not_actionable", "Az ajánlat nem dealelhető / Quote not actionable"],
    ["not_staged", "Az ajánlat nincs staged állapotban / Quote not staged"],
  ])("code %s renders the bilingual title %s", (code, expected) => {
    expect(dealSagaErrorTitle(code)).toBe(expected);
  });

  it("unknown / unmapped code falls through to the generic toast", () => {
    expect(dealSagaErrorTitle("unknown")).toBe("A DEAL sikertelen / DEAL failed");
    expect(dealSagaErrorTitle("future_kind_we_never_saw")).toBe(
      "A DEAL sikertelen / DEAL failed",
    );
  });
});
