// PR-65 / session-86 — pin tests for the SPA's list-row Tier-1
// UX helpers (Partner column fallback + row-quick-action gating).
//
// Mirror invariant per A161 / A163: the row-quick-action button
// table MUST stay a strict subset of the detail-modal button
// table from `buttonsForState`. A regression that surfaced a
// row-level button on a state where the backend's precondition
// guard would 409 (e.g., Storno on Ready) would produce the
// failure mode A157 inline error renders — but only AFTER an
// operator-visible click that wasted their time. CLAUDE.md rule
// 12: pin the gating loud at the pure-helper layer.
//
// CLAUDE.md rule 9 — per-state coverage means a regression that
// collapses every state to one quick-action set (or returns a
// constant) cannot pass every assertion vacuously.

import { describe, expect, it } from "vitest";

import {
  PARTNER_COLUMN_EM_DASH,
  buyerColumnDisplay,
  quickActionMeta,
  quickActionsForState,
  type RowQuickAction,
} from "./invoice-list";
import { buttonsForState } from "./invoice-actions";
import type { InvoiceState } from "./api";

// ── buyerColumnDisplay ──────────────────────────────────────────

describe("buyerColumnDisplay", () => {
  it("returns the trimmed name when the backend supplied one", () => {
    // Happy path — partner-selected buyer (the issuance composer
    // writes `partner.legal_name` into the side-store's
    // `customer.name`). The display surfaces it verbatim modulo
    // trim; the backend already trims via
    // `read_buyer_name_from_side_store`, but the trim here is
    // defence in depth against any future wire-shape that bypasses
    // the helper.
    expect(buyerColumnDisplay("Budapesti Sport-Egyesület Kft.")).toBe(
      "Budapesti Sport-Egyesület Kft.",
    );
    expect(buyerColumnDisplay("  BSCE  ")).toBe("BSCE");
  });

  it("returns the em-dash placeholder for null (CLI-issued / pre-PR-47α invoices)", () => {
    // The backend returns `null` for invoices with no side-store
    // companion: CLI-issued (`aberp issue-invoice --in ...` never
    // wrote the snapshot) and pre-PR-47α SPA-issued. The SPA must
    // render a quiet em-dash, NOT an empty cell — an empty cell is
    // ambiguous with "data not yet loaded" and would erode operator
    // trust in the column.
    expect(buyerColumnDisplay(null)).toBe(PARTNER_COLUMN_EM_DASH);
    expect(PARTNER_COLUMN_EM_DASH).toBe("—");
  });

  it("returns the em-dash placeholder for a blank or whitespace-only name", () => {
    // Defence in depth: even though `read_buyer_name_from_side_store`
    // collapses blank values to None on the Rust side (pinned by
    // `read_buyer_name_from_side_store_round_trip`), the SPA must
    // not fabricate an empty cell on a future wire-shape that
    // forwards a `""` from a different code path.
    expect(buyerColumnDisplay("")).toBe(PARTNER_COLUMN_EM_DASH);
    expect(buyerColumnDisplay("   ")).toBe(PARTNER_COLUMN_EM_DASH);
    expect(buyerColumnDisplay("\t\n")).toBe(PARTNER_COLUMN_EM_DASH);
  });
});

// ── quickActionsForState ────────────────────────────────────────

interface QuickActionExpectation {
  state: InvoiceState;
  actions: RowQuickAction[];
}

// The eleven InvoiceState labels per ADR-0036 §2, with the
// row-quick-action subset of each state's detail-modal vocab. PR-65
// surfaces only Download / Submit / Storno at the row level — PollAck
// and Modification stay detail-modal-only by design (Modification
// opens a fresh form; PollAck benefits from the modal's larger error
// surface during the bounded 31s loop).
const QUICK_ACTION_TABLE: QuickActionExpectation[] = [
  // Ready — operator can submit OR download.
  { state: "Ready", actions: ["Download", "Submit"] },
  // Submitted — PollAck is detail-modal-only; row keeps Download.
  { state: "Submitted", actions: ["Download"] },
  // PendingNavExists — same posture as Submitted at the row level.
  { state: "PendingNavExists", actions: ["Download"] },
  // Pending — recovery-only state; row keeps Download.
  { state: "Pending", actions: ["Download"] },
  // Recovered — terminal-like for row-level affordances; Download only.
  { state: "Recovered", actions: ["Download"] },
  // Finalized — operator can cancel (storno) OR download. Modification
  // is excluded from row quick actions (opens a fresh form per ADR-0024).
  //
  // PR-70 / ADR-0039 — the default (paid=false) baseline includes the
  // new "Pay" row-quick-action between Submit and Storno. The paid=true
  // branch is pinned separately by `finalized_paid_hides_pay_action`.
  { state: "Finalized", actions: ["Download", "Pay", "Storno"] },
  // Rejected — terminal; Download only.
  { state: "Rejected", actions: ["Download"] },
  // Storno — base has a storno chain entry; Download only.
  { state: "Storno", actions: ["Download"] },
  // Amended — base has a modification chain entry; row-level
  // Modification not surfaced (form-driven).
  { state: "Amended", actions: ["Download"] },
  // Abandoned — operator marked terminal; Download only.
  { state: "Abandoned", actions: ["Download"] },
  // Unknown — no entries; Download still surfaced (it will 404,
  // operator-visible per CLAUDE.md rule 12).
  { state: "Unknown", actions: ["Download"] },
];

describe("quickActionsForState", () => {
  for (const { state, actions } of QUICK_ACTION_TABLE) {
    it(`returns [${actions.join(", ")}] for state=${state}`, () => {
      expect(quickActionsForState(state)).toEqual(actions);
    });
  }

  it("Submit quick-action only appears on Ready", () => {
    // Mirror of `buttonsForState`'s Submit pin: surfacing Submit on
    // a post-submission state would produce a 409 the operator was
    // not warned about. The row-level surface inherits the gating
    // verbatim from `buttonsForState`.
    const statesWithSubmit = QUICK_ACTION_TABLE.filter((row) =>
      row.actions.includes("Submit"),
    ).map((row) => row.state);
    expect(statesWithSubmit).toEqual(["Ready"]);
  });

  it("Storno quick-action only appears on Finalized", () => {
    // PR-47α / ADR-0023 §1: Storno requires a terminal SAVED ack.
    // The row-level surface must keep this gating; surfacing it on
    // Amended (the modify-after-modify base) would diverge from the
    // backend's `check_base_is_finalized` classifier.
    const statesWithStorno = QUICK_ACTION_TABLE.filter((row) =>
      row.actions.includes("Storno"),
    ).map((row) => row.state);
    expect(statesWithStorno).toEqual(["Finalized"]);
  });

  it("Download quick-action is present on every state", () => {
    // A155 / PR-44ε.UI: the printed PDF exists from the moment the
    // draft is created. Hiding the row-level download for any
    // lifecycle state would strand the operator without the
    // operator-deliverable artifact at scan time.
    for (const { state, actions } of QUICK_ACTION_TABLE) {
      expect(
        actions.includes("Download"),
        `state=${state} must include Download`,
      ).toBe(true);
    }
  });

  it("row-level quick actions are a strict subset of buttonsForState", () => {
    // Mirror invariant: the row vocab is narrow by design (PollAck +
    // Modification stay detail-modal-only). A regression that
    // surfaced a button at the row level which the detail modal
    // refused to render would diverge two surfaces operators expect
    // to agree.
    for (const { state, actions } of QUICK_ACTION_TABLE) {
      const detailButtons = buttonsForState(state);
      for (const action of actions) {
        expect(
          detailButtons.includes(action),
          `row action=${action} on state=${state} must also appear in buttonsForState`,
        ).toBe(true);
      }
    }
  });

  it("never surfaces PollAck or Modification at the row level", () => {
    // Counter-pin: a future refactor that widened RowQuickAction to
    // include PollAck or Modification would break the brief's
    // explicit row vocab (`📄 PDF / ↗ Submit / ⊘ Storno`). The
    // module's exported type already excludes both at compile time;
    // this assertion catches a runtime regression that bypassed the
    // type.
    for (const { state, actions } of QUICK_ACTION_TABLE) {
      const widened = actions as readonly string[];
      expect(widened.includes("PollAck"), `state=${state} must not include PollAck`).toBe(false);
      expect(widened.includes("Modification"), `state=${state} must not include Modification`).toBe(false);
    }
  });

  it("covers every InvoiceState union member", () => {
    const stateNames = QUICK_ACTION_TABLE.map((row) => row.state).sort();
    const expected: InvoiceState[] = [
      "Abandoned",
      "Amended",
      "Finalized",
      "Pending",
      "PendingNavExists",
      "Ready",
      "Recovered",
      "Rejected",
      "Storno",
      "Submitted",
      "Unknown",
    ];
    expect(stateNames).toEqual(expected);
  });

  // ── PR-70 / ADR-0039 — Pay quick-action gating ────────────────────

  it("Pay quick-action appears on Finalized when paid=false", () => {
    // The unpaid baseline mirrors the default-paid=false fixture in
    // the table above. Explicit-paid=false pin so a regression that
    // ignored the parameter surfaces.
    expect(quickActionsForState("Finalized", false)).toEqual([
      "Download",
      "Pay",
      "Storno",
    ]);
  });

  it("Pay quick-action is hidden on Finalized when paid=true", () => {
    // Paid branch — operator cannot re-pay an invoice (backend
    // enforces no-double-pay with 409 per ADR-0039 §3). Surfacing
    // the row-level Pay button on a paid invoice would mirror the
    // detail-modal regression named in
    // `invoice-actions.test.ts::finalized_paid_hides_pay_button`.
    expect(quickActionsForState("Finalized", true)).toEqual([
      "Download",
      "Storno",
    ]);
  });

  it("Pay quick-action never appears on non-Finalized states", () => {
    // Counter-pin mirroring the detail-modal Pay gating.
    const nonFinalized: InvoiceState[] = [
      "Unknown",
      "Ready",
      "Pending",
      "PendingNavExists",
      "Submitted",
      "Recovered",
      "Rejected",
      "Storno",
      "Amended",
      "Abandoned",
    ];
    for (const state of nonFinalized) {
      expect(quickActionsForState(state, false).includes("Pay")).toBe(false);
      expect(quickActionsForState(state, true).includes("Pay")).toBe(false);
    }
  });
});

// ── quickActionMeta ─────────────────────────────────────────────

describe("quickActionMeta", () => {
  it("returns the operator-facing glyph + label for each action", () => {
    // The glyphs mirror the brief's explicit `📄 PDF / ↗ Submit /
    // ⊘ Storno` placement. A drift here would mean the row's
    // categorical signal (glyph) diverges from the operator's
    // documented expectation; ADR-0017 §"Adversarial review #4"
    // names the glyph as the colour-blind-safe signal carrier.
    expect(quickActionMeta("Download")).toEqual({
      glyph: "📄",
      label: "Download PDF",
    });
    expect(quickActionMeta("Submit")).toEqual({
      glyph: "↗",
      label: "Submit to NAV",
    });
    expect(quickActionMeta("Storno")).toEqual({
      glyph: "⊘",
      label: "Cancel (storno)",
    });
    // PR-70 / ADR-0039 — the mark-as-paid quick action.
    expect(quickActionMeta("Pay")).toEqual({
      glyph: "💰",
      label: "Mark as paid",
    });
  });
});
