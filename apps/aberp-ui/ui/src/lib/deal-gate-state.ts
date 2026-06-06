// S272 / PR-261 — pure-function helper extracted from QuoteDealGate
// so vitest can pin the EVE-addendum-2 (UI half) + addendum-3 visual
// contract without spinning a Svelte renderer.
//
// The Svelte component imports `deriveDealGateState` and feeds the
// returned tone + can-submit + expected-token strings into its
// reactive `$state`/`$derived` cells. The visual contract — RED border
// when empty/wrong, GREEN when matching, server-side defense-in-depth
// for the submit gate — is testable from this one function.
//
// Per [[hulye-biztos]] all comparisons are case-sensitive: the input
// `REFRESH` is not auto-uppercased and the DEAL token is verbatim.

import type { DealSagaErrorCode } from "./api";

/** Border tier per EVE addendum 3. */
export type DealGateTone = "empty" | "wrong" | "correct";

export interface DealGateInput {
  /** The row's `quote_id`. The expected token is its first 8 chars. */
  quoteId: string;
  /** Row's current `stock_alert` — drives REFRESH-gate visibility. */
  stockAlert: boolean;
  /** Operator-typed REFRESH input. NOT auto-uppercased. */
  refreshInput: string;
  /** Operator-typed DEAL token. NOT auto-uppercased; case-sensitive. */
  dealInput: string;
  /** `true` while an in-flight POST is being awaited. Disables submit. */
  submitting: boolean;
}

export interface DealGateState {
  /** First 8 chars of `quoteId` verbatim (or whole id if shorter). */
  expectedDealToken: string;
  /** `true` iff `refreshInput === "REFRESH"`. */
  refreshAcked: boolean;
  /** `true` iff `dealInput === expectedDealToken`. */
  dealTokenMatches: boolean;
  /** Border tier: RED on empty/wrong, GREEN on correct. */
  dealTone: DealGateTone;
  /** Composite gate: REFRESH satisfied (if stockAlert) + token correct
   * + not currently submitting. */
  canSubmit: boolean;
}

/** Cap on the DEAL token comparison window. Mirrors
 * `aberp::quote_deal::QUOTE_DEAL_TOKEN_LEN`. */
export const QUOTE_DEAL_TOKEN_LEN = 8;

/** Literal REFRESH token. Mirrors `aberp::quote_deal::REFRESH_ACK_TOKEN`. */
export const REFRESH_ACK_TOKEN = "REFRESH";

export function expectedDealToken(quoteId: string): string {
  const take = Math.min(QUOTE_DEAL_TOKEN_LEN, quoteId.length);
  return quoteId.slice(0, take);
}

export function deriveDealGateState(input: DealGateInput): DealGateState {
  const expected = expectedDealToken(input.quoteId);
  const refreshAcked = input.refreshInput === REFRESH_ACK_TOKEN;
  const dealTokenMatches = input.dealInput === expected;
  const dealTone: DealGateTone =
    input.dealInput.length === 0 ? "empty" : dealTokenMatches ? "correct" : "wrong";
  const canSubmit =
    (!input.stockAlert || refreshAcked) && dealTokenMatches && !input.submitting;
  return {
    expectedDealToken: expected,
    refreshAcked,
    dealTokenMatches,
    dealTone,
    canSubmit,
  };
}

/** Map the closed-vocab 409 machine code (or `unknown`) to bilingual
 * operator-facing copy. Pinned in the vitest pin so a future PR adding
 * a machine code surfaces the missing arm here (`undefined` fall-through
 * → snapshot diff) rather than reaching prod as a blank toast. */
export function dealSagaErrorTitle(code: DealSagaErrorCode | "unknown" | string): string {
  switch (code) {
    case "stock_alert_refresh_required":
      return "REFRESH kötelező / REFRESH required";
    case "deal_already_issued":
      return "Ez a DEAL már kiállításra került / DEAL already issued";
    case "deal_token_mismatch":
      return "Token nem egyezik / Token does not match";
    case "not_actionable":
      return "Az ajánlat nem dealelhető / Quote not actionable";
    case "not_staged":
      return "Az ajánlat nincs staged állapotban / Quote not staged";
    default:
      return "A DEAL sikertelen / DEAL failed";
  }
}
