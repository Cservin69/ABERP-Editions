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
  EMPTY_FILTER,
  PARTNER_COLUMN_EM_DASH,
  buyerColumnDisplay,
  canOpenDetail,
  compareInvoices,
  filterInvoices,
  isFilterEmpty,
  quickActionMeta,
  quickActionsForState,
  type InvoiceFilterSpec,
  type InvoiceSortRow,
  type RowQuickAction,
  type SortKey,
} from "./invoice-list";
import { buttonsForState } from "./invoice-actions";
import type { Currency, InvoiceState } from "./api";

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
// surfaces only Download / Submit / Storno at the row level —
// Modification stays detail-modal-only by design (it opens a fresh
// form). PR-95 / session-115 — the poll affordance moved off the
// button surface entirely onto the NAV-status pictogram in the
// state column, so the row's quick-action column never carries it.
const QUICK_ACTION_TABLE: QuickActionExpectation[] = [
  // Ready — operator can submit OR download.
  { state: "Ready", actions: ["Download", "Submit"] },
  // Submitted — pictogram (state column) carries the poll affordance;
  // row's quick-action column keeps Download only.
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
  // S236 / S239 — pre-allocation Draft rows surface only the
  // S239 Delete quick-action (cascades the dispatch's
  // spawned_invoice_id pointer per S237 §🔴 #1).
  { state: "Draft", actions: ["Delete"] },
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

  it("Download quick-action is present on every allocated state", () => {
    // A155 / PR-44ε.UI: the printed PDF exists from the moment an
    // `inv_*` row is allocated. Hiding the row-level download for
    // any allocated lifecycle state would strand the operator
    // without the operator-deliverable artifact at scan time. The
    // pre-allocation Draft state has no PDF yet and is explicitly
    // skipped here.
    for (const { state, actions } of QUICK_ACTION_TABLE) {
      if (state === "Draft") continue;
      expect(
        actions.includes("Download"),
        `state=${state} must include Download`,
      ).toBe(true);
    }
  });

  it("row-level quick actions are a strict subset of buttonsForState", () => {
    // Mirror invariant: the row vocab is narrow by design
    // (Modification stays detail-modal-only). A regression that
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

  it("never surfaces Modification at the row level", () => {
    // Counter-pin: a future refactor that widened RowQuickAction to
    // include Modification would break the brief's explicit row
    // vocab (`📄 PDF / ↗ Submit / 💰 Pay / ⊘ Storno`). The module's
    // exported type already excludes it at compile time; this
    // assertion catches a runtime regression that bypassed the type.
    // (PR-95 dropped PollAck from the wider button vocab entirely;
    // there is nothing to counter-pin for it here any more.)
    for (const { state, actions } of QUICK_ACTION_TABLE) {
      const widened = actions as readonly string[];
      expect(
        widened.includes("Modification"),
        `state=${state} must not include Modification`,
      ).toBe(false);
    }
  });

  it("covers every InvoiceState union member", () => {
    const stateNames = QUICK_ACTION_TABLE.map((row) => row.state).sort();
    const expected: InvoiceState[] = [
      "Abandoned",
      "Amended",
      "Draft",
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
      "Draft",
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
    // S239 / PR-233 — the pre-allocation draft Delete action.
    expect(quickActionMeta("Delete")).toEqual({
      glyph: "🗑",
      label: "Delete draft",
    });
  });
});

// ── PR-94 / session-114 — sortable columns + facet filter ───────
//
// Pin discipline:
//   - Per column + both directions (asc/desc).
//   - Ties broken by invoice_id ascending, regardless of dir.
//   - Nulls last for the optional columns (partner, total), regardless
//     of dir.
//   - Each facet + combinations; needle still works through the
//     composed helper.

function row(
  partial: Partial<InvoiceSortRow> & { invoice_id: string },
): InvoiceSortRow {
  return {
    sequence_number: 1,
    fiscal_year: 2026,
    state: "Ready",
    total_gross: 1000,
    buyer_name: null,
    currency: "HUF",
    row_kind: "Own",
    ...partial,
  };
}

describe("compareInvoices — invoice_id column", () => {
  it("sorts ascending by ULID lex order", () => {
    const rows = [
      row({ invoice_id: "01J0C" }),
      row({ invoice_id: "01J0A" }),
      row({ invoice_id: "01J0B" }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "invoice_id", "asc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["01J0A", "01J0B", "01J0C"]);
  });
  it("sorts descending by ULID lex order", () => {
    const rows = [
      row({ invoice_id: "01J0C" }),
      row({ invoice_id: "01J0A" }),
      row({ invoice_id: "01J0B" }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "invoice_id", "desc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["01J0C", "01J0B", "01J0A"]);
  });
});

describe("compareInvoices — invoice_number column", () => {
  it("orders by fiscal_year then sequence_number ascending", () => {
    // The composed `YYYY-NNNNNN` is operator-meaningful; the tuple
    // compare (year, seq) is the load-bearing contract, NOT lex on
    // the composed string. Mixing fiscal years exercises the year-
    // first arm.
    const rows = [
      row({ invoice_id: "B", fiscal_year: 2026, sequence_number: 1 }),
      row({ invoice_id: "A", fiscal_year: 2025, sequence_number: 999 }),
      row({ invoice_id: "C", fiscal_year: 2026, sequence_number: 2 }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "invoice_number", "asc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["A", "B", "C"]);
  });
  it("descending flips the natural order", () => {
    const rows = [
      row({ invoice_id: "B", fiscal_year: 2026, sequence_number: 1 }),
      row({ invoice_id: "A", fiscal_year: 2025, sequence_number: 999 }),
      row({ invoice_id: "C", fiscal_year: 2026, sequence_number: 2 }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "invoice_number", "desc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["C", "B", "A"]);
  });
});

describe("compareInvoices — partner column", () => {
  it("sorts non-null partners alphabetically (locale-aware)", () => {
    const rows = [
      row({ invoice_id: "C", buyer_name: "Charlie Kft." }),
      row({ invoice_id: "A", buyer_name: "Alpha Kft." }),
      row({ invoice_id: "B", buyer_name: "Bravo Kft." }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "partner", "asc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["A", "B", "C"]);
  });
  it("places null partner rows AFTER named rows in ascending dir", () => {
    const rows = [
      row({ invoice_id: "Z", buyer_name: null }),
      row({ invoice_id: "A", buyer_name: "Alpha Kft." }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "partner", "asc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["A", "Z"]);
  });
  it("places null partner rows AFTER named rows in descending dir too", () => {
    // Nulls-last regardless of direction — spreadsheet convention.
    // A regression that flipped null-side with dir would shuffle the
    // em-dash cluster to the TOP on a desc click, breaking the
    // operator's mental model that 'descending = same data, reversed'.
    const rows = [
      row({ invoice_id: "Z", buyer_name: null }),
      row({ invoice_id: "A", buyer_name: "Alpha Kft." }),
      row({ invoice_id: "B", buyer_name: "Bravo Kft." }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "partner", "desc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["B", "A", "Z"]);
  });
  it("treats blank-or-whitespace partner as null for sort purposes", () => {
    // Mirror of `buyerColumnDisplay`'s fallback contract.
    const rows = [
      row({ invoice_id: "Y", buyer_name: "   " }),
      row({ invoice_id: "Z", buyer_name: "" }),
      row({ invoice_id: "A", buyer_name: "Alpha Kft." }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "partner", "asc"));
    // Blank and empty both treated as null → sort to bottom; their
    // relative order goes to the invoice_id tiebreaker (Y < Z).
    expect(rows.map((r) => r.invoice_id)).toEqual(["A", "Y", "Z"]);
  });
});

describe("compareInvoices — series_number column", () => {
  it("sorts numerically ascending (not lex)", () => {
    // A regression that compared as strings would order 1 < 10 < 2;
    // the numeric sort must put 2 between 1 and 10.
    const rows = [
      row({ invoice_id: "C", sequence_number: 10 }),
      row({ invoice_id: "A", sequence_number: 1 }),
      row({ invoice_id: "B", sequence_number: 2 }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "series_number", "asc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["A", "B", "C"]);
  });
  it("descending reverses the numeric order", () => {
    const rows = [
      row({ invoice_id: "C", sequence_number: 10 }),
      row({ invoice_id: "A", sequence_number: 1 }),
      row({ invoice_id: "B", sequence_number: 2 }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "series_number", "desc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["C", "B", "A"]);
  });
});

describe("compareInvoices — fiscal_year column", () => {
  it("sorts ascending then descending numerically", () => {
    const rows = [
      row({ invoice_id: "C", fiscal_year: 2026 }),
      row({ invoice_id: "A", fiscal_year: 2024 }),
      row({ invoice_id: "B", fiscal_year: 2025 }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "fiscal_year", "asc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["A", "B", "C"]);
    rows.sort((a, b) => compareInvoices(a, b, "fiscal_year", "desc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["C", "B", "A"]);
  });
});

describe("compareInvoices — state column", () => {
  it("sorts by LIFECYCLE_ORDER ascending (Unknown → Abandoned)", () => {
    // Mirrors `labels.ts::LIFECYCLE_ORDER`. A regression that fell
    // through to alphabetical would put `Abandoned` before
    // `Amended` before `Finalized` — the exact bucket the
    // lifecycle-natural sort exists to avoid.
    const rows = [
      row({ invoice_id: "D", state: "Abandoned" }),
      row({ invoice_id: "A", state: "Ready" }),
      row({ invoice_id: "C", state: "Storno" }),
      row({ invoice_id: "B", state: "Finalized" }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "state", "asc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["A", "B", "C", "D"]);
  });
  it("sorts by LIFECYCLE_ORDER descending (Abandoned → Unknown)", () => {
    const rows = [
      row({ invoice_id: "D", state: "Abandoned" }),
      row({ invoice_id: "A", state: "Ready" }),
      row({ invoice_id: "C", state: "Storno" }),
      row({ invoice_id: "B", state: "Finalized" }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "state", "desc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["D", "C", "B", "A"]);
  });
});

describe("compareInvoices — total column", () => {
  it("sorts by total_gross numerically by minor units (not string lex)", () => {
    // The load-bearing pin per the brief: "numeric total by minor-units
    // not string". A regression that compared total_gross as a string
    // would put "1000" < "9" — the bug the brief explicitly names.
    const rows = [
      row({ invoice_id: "C", total_gross: 1000 }),
      row({ invoice_id: "A", total_gross: 9 }),
      row({ invoice_id: "B", total_gross: 100 }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "total", "asc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["A", "B", "C"]);
  });
  it("descending reverses numeric order", () => {
    const rows = [
      row({ invoice_id: "C", total_gross: 1000 }),
      row({ invoice_id: "A", total_gross: 9 }),
      row({ invoice_id: "B", total_gross: 100 }),
    ];
    rows.sort((a, b) => compareInvoices(a, b, "total", "desc"));
    expect(rows.map((r) => r.invoice_id)).toEqual(["C", "B", "A"]);
  });
  it("places null totals AFTER non-null in both directions", () => {
    // Null-total = draft with no amount; spreadsheet-convention
    // nulls-last regardless of dir. Operator's mental model: meaningful
    // values cluster at the top; draftless rows fall through.
    const rowsAsc = [
      row({ invoice_id: "Z", total_gross: null }),
      row({ invoice_id: "A", total_gross: 9 }),
    ];
    rowsAsc.sort((a, b) => compareInvoices(a, b, "total", "asc"));
    expect(rowsAsc.map((r) => r.invoice_id)).toEqual(["A", "Z"]);
    const rowsDesc = [
      row({ invoice_id: "Z", total_gross: null }),
      row({ invoice_id: "A", total_gross: 9 }),
      row({ invoice_id: "B", total_gross: 1000 }),
    ];
    rowsDesc.sort((a, b) => compareInvoices(a, b, "total", "desc"));
    expect(rowsDesc.map((r) => r.invoice_id)).toEqual(["B", "A", "Z"]);
  });
});

describe("compareInvoices — ties + stability", () => {
  it("breaks ties by invoice_id ascending regardless of dir", () => {
    // Three rows with identical sort key + dir asc → invoice_id asc.
    const ascRows = [
      row({ invoice_id: "C", sequence_number: 5 }),
      row({ invoice_id: "A", sequence_number: 5 }),
      row({ invoice_id: "B", sequence_number: 5 }),
    ];
    ascRows.sort((a, b) => compareInvoices(a, b, "series_number", "asc"));
    expect(ascRows.map((r) => r.invoice_id)).toEqual(["A", "B", "C"]);
    // Same ties + dir desc → invoice_id ASC (tiebreaker NEVER flips).
    // A regression that flipped the tiebreaker would re-shuffle the
    // operator's display on every dir toggle for tied rows.
    const descRows = [
      row({ invoice_id: "C", sequence_number: 5 }),
      row({ invoice_id: "A", sequence_number: 5 }),
      row({ invoice_id: "B", sequence_number: 5 }),
    ];
    descRows.sort((a, b) => compareInvoices(a, b, "series_number", "desc"));
    expect(descRows.map((r) => r.invoice_id)).toEqual(["A", "B", "C"]);
  });
});

describe("compareInvoices — coverage", () => {
  it("covers every SortKey vocab member", () => {
    // Exhaustiveness counter-pin: a future SortKey member must add a
    // switch arm in `compareInvoices` (TS-enforced) AND show up here
    // (runtime-enforced). The two-layer pin matches the LIFECYCLE_ORDER
    // / LABELS guard in labels.ts.
    const keys: SortKey[] = [
      "invoice_id",
      "invoice_number",
      "partner",
      "series_number",
      "fiscal_year",
      "state",
      "total",
    ];
    const sample = [
      row({ invoice_id: "A" }),
      row({ invoice_id: "B" }),
    ];
    for (const k of keys) {
      // No throw + a number result for each key on a 2-row corpus.
      const c = compareInvoices(sample[0], sample[1], k, "asc");
      expect(typeof c).toBe("number");
    }
  });
});

// ── filterInvoices ─────────────────────────────────────────────

function frow(
  partial: Partial<InvoiceSortRow> & { invoice_id: string },
): InvoiceSortRow {
  return row(partial);
}

const FIXTURES: InvoiceSortRow[] = [
  frow({
    invoice_id: "01R001",
    sequence_number: 1,
    fiscal_year: 2026,
    state: "Ready",
    total_gross: 1000,
    buyer_name: "Alpha Kft.",
    currency: "HUF",
  }),
  frow({
    invoice_id: "01F002",
    sequence_number: 2,
    fiscal_year: 2026,
    state: "Finalized",
    total_gross: 5000,
    buyer_name: "Bravo Kft.",
    currency: "HUF",
  }),
  frow({
    invoice_id: "01E003",
    sequence_number: 3,
    fiscal_year: 2026,
    state: "Finalized",
    total_gross: 10000,
    buyer_name: "Charlie Kft.",
    currency: "EUR",
  }),
  frow({
    invoice_id: "01S004",
    sequence_number: 4,
    fiscal_year: 2026,
    state: "Storno",
    total_gross: 5000,
    buyer_name: "Delta Kft.",
    currency: "HUF",
  }),
];

describe("filterInvoices — facet gating", () => {
  it("returns every row when every facet is open (EMPTY_FILTER)", () => {
    const out = filterInvoices(FIXTURES, EMPTY_FILTER);
    expect(out.length).toBe(FIXTURES.length);
    expect(out.map((r) => r.invoice_id)).toEqual([
      "01R001",
      "01F002",
      "01E003",
      "01S004",
    ]);
  });
  it("state facet narrows to matching state only", () => {
    const out = filterInvoices(FIXTURES, {
      needle: "",
      state: "Finalized",
      currency: "All",
      row_kind: "All",
    });
    expect(out.map((r) => r.invoice_id)).toEqual(["01F002", "01E003"]);
  });
  it("currency facet narrows to matching currency only", () => {
    const out = filterInvoices(FIXTURES, {
      needle: "",
      state: "All",
      currency: "EUR",
      row_kind: "All",
    });
    expect(out.map((r) => r.invoice_id)).toEqual(["01E003"]);
  });
  it("state + currency facets AND together", () => {
    const out = filterInvoices(FIXTURES, {
      needle: "",
      state: "Finalized",
      currency: "HUF",
      row_kind: "All",
    });
    // Finalized AND HUF — only 01F002 qualifies (01E003 is Finalized
    // but EUR).
    expect(out.map((r) => r.invoice_id)).toEqual(["01F002"]);
  });
  it("needle ANDs with both facets", () => {
    // Needle "alpha" matches buyer_name "Alpha Kft." (Ready/HUF). When
    // facets gate to Finalized/HUF, the needle has no match → empty.
    const noMatch = filterInvoices(FIXTURES, {
      needle: "alpha",
      state: "Finalized",
      currency: "HUF",
      row_kind: "All",
    });
    expect(noMatch.length).toBe(0);
    // Same needle with All facets surfaces the Alpha row.
    const match = filterInvoices(FIXTURES, {
      needle: "alpha",
      state: "All",
      currency: "All",
      row_kind: "All",
    });
    expect(match.map((r) => r.invoice_id)).toEqual(["01R001"]);
  });
  it("needle finds composed invoice number across facets", () => {
    // PR-68 contract — substring search across composed `YYYY-NNNNNN`.
    const out = filterInvoices(FIXTURES, {
      needle: "2026-000002",
      state: "All",
      currency: "All",
      row_kind: "All",
    });
    expect(out.map((r) => r.invoice_id)).toEqual(["01F002"]);
  });
});

describe("filterInvoices — every currency facet value passes through", () => {
  // Counter-pin per CLAUDE.md rule 9 — a regression collapsing the
  // currency arm to always-true (or always-false) would slip through
  // a single-fixture test. Probe each Currency value individually.
  const cases: Array<{ currency: Currency; expected: string[] }> = [
    { currency: "HUF", expected: ["01R001", "01F002", "01S004"] },
    { currency: "EUR", expected: ["01E003"] },
  ];
  for (const c of cases) {
    it(`currency=${c.currency} surfaces only matching rows`, () => {
      const out = filterInvoices(FIXTURES, {
        needle: "",
        state: "All",
        currency: c.currency,
        row_kind: "All",
      });
      expect(out.map((r) => r.invoice_id)).toEqual(c.expected);
    });
  }
});

describe("filterInvoices — needle does not bypass the facets", () => {
  it("needle 'finalized' alone surfaces both Finalized rows", () => {
    const spec: InvoiceFilterSpec = {
      needle: "finalized",
      state: "All",
      currency: "All",
      row_kind: "All",
    };
    const out = filterInvoices(FIXTURES, spec);
    expect(out.map((r) => r.invoice_id)).toEqual(["01F002", "01E003"]);
  });
  it("but a currency facet still narrows the needle hits", () => {
    const spec: InvoiceFilterSpec = {
      needle: "finalized",
      state: "All",
      currency: "EUR",
      row_kind: "All",
    };
    const out = filterInvoices(FIXTURES, spec);
    expect(out.map((r) => r.invoice_id)).toEqual(["01E003"]);
  });
});

describe("isFilterEmpty", () => {
  it("returns true for the EMPTY_FILTER constant", () => {
    expect(isFilterEmpty(EMPTY_FILTER)).toBe(true);
  });
  it("returns true for whitespace-only needle + All facets", () => {
    expect(
      isFilterEmpty({ needle: "   ", state: "All", currency: "All", row_kind: "All" }),
    ).toBe(true);
  });
  it("returns false when any facet is engaged", () => {
    expect(
      isFilterEmpty({ needle: "x", state: "All", currency: "All", row_kind: "All" }),
    ).toBe(false);
    expect(
      isFilterEmpty({ needle: "", state: "Finalized", currency: "All", row_kind: "All" }),
    ).toBe(false);
    expect(
      isFilterEmpty({ needle: "", state: "All", currency: "EUR", row_kind: "All" }),
    ).toBe(false);
    // PR-213 / S215 — row_kind facet engages isFilterEmpty's gate too.
    expect(
      isFilterEmpty({ needle: "", state: "All", currency: "All", row_kind: "Own" }),
    ).toBe(false);
    expect(
      isFilterEmpty({ needle: "", state: "All", currency: "All", row_kind: "ExtNav" }),
    ).toBe(false);
  });
});

// ── PR-213 / S215 — row_kind sort + filter ─────────────────────────
//
// The virtual-union shape per ADR-0058 lives ONLY at the wire-shape +
// render-component layer; the sort/filter helpers here are the
// load-bearing client-side machinery. Three properties pinned:
//   1. compareInvoices on row_kind orders Own before ExtNav ascending.
//   2. compareInvoices descending flips that order.
//   3. filterInvoices on row_kind="ExtNav" surfaces only ExtNav rows;
//      "Own" surfaces only Own rows.
// A regression that defaults missing row_kind to "Own" silently (or
// vice versa) would slip past a single-fixture test; the mixed-kind
// fixture below makes the comparator and the facet gate impossible to
// hard-code to one constant.

const ROW_KIND_FIXTURES: InvoiceSortRow[] = [
  frow({
    invoice_id: "01OWN001",
    sequence_number: 1,
    fiscal_year: 2026,
    state: "Finalized",
    total_gross: 1000,
    buyer_name: "Alpha Kft.",
    currency: "HUF",
    row_kind: "Own",
  }),
  frow({
    invoice_id: "rinv_NAV0042",
    sequence_number: 0,
    fiscal_year: 2026,
    state: "Unknown",
    total_gross: 3000,
    buyer_name: null,
    currency: "HUF",
    row_kind: "ExtNav",
  }),
  frow({
    invoice_id: "01OWN002",
    sequence_number: 2,
    fiscal_year: 2026,
    state: "Ready",
    total_gross: 2000,
    buyer_name: "Bravo Kft.",
    currency: "HUF",
    row_kind: "Own",
  }),
  frow({
    invoice_id: "rinv_NAV0099",
    sequence_number: 0,
    fiscal_year: 2026,
    state: "Unknown",
    total_gross: 5000,
    buyer_name: null,
    currency: "EUR",
    row_kind: "ExtNav",
  }),
];

describe("compareInvoices — row_kind column", () => {
  it("sorts Own before ExtNav ascending", () => {
    const rows = [...ROW_KIND_FIXTURES];
    rows.sort((a, b) => compareInvoices(a, b, "row_kind", "asc"));
    // Both Own rows cluster first; both ExtNav rows cluster second.
    // Within each cluster, the invoice_id tiebreaker (ascending) sets
    // the inner order regardless of the user-selected dir.
    expect(rows.map((r) => r.invoice_id)).toEqual([
      "01OWN001",
      "01OWN002",
      "rinv_NAV0042",
      "rinv_NAV0099",
    ]);
  });
  it("sorts ExtNav before Own descending", () => {
    const rows = [...ROW_KIND_FIXTURES];
    rows.sort((a, b) => compareInvoices(a, b, "row_kind", "desc"));
    // ExtNav cluster first; Own cluster second. Within each cluster
    // the invoice_id tiebreak stays ASCENDING (the tiebreak does NOT
    // flip with dir — see `compareInvoices — ties + stability`).
    expect(rows.map((r) => r.invoice_id)).toEqual([
      "rinv_NAV0042",
      "rinv_NAV0099",
      "01OWN001",
      "01OWN002",
    ]);
  });
});

describe("filterInvoices — row_kind facet", () => {
  it('row_kind="All" surfaces every row', () => {
    const out = filterInvoices(ROW_KIND_FIXTURES, {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "All",
    });
    expect(out.length).toBe(ROW_KIND_FIXTURES.length);
  });
  it('row_kind="Own" hides every ExtNav row', () => {
    const out = filterInvoices(ROW_KIND_FIXTURES, {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "Own",
    });
    expect(out.map((r) => r.invoice_id)).toEqual(["01OWN001", "01OWN002"]);
  });
  it('row_kind="ExtNav" hides every Own row', () => {
    const out = filterInvoices(ROW_KIND_FIXTURES, {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "ExtNav",
    });
    expect(out.map((r) => r.invoice_id)).toEqual(["rinv_NAV0042", "rinv_NAV0099"]);
  });
  it("row_kind ANDs with currency facet (ExtNav + EUR isolates one row)", () => {
    const out = filterInvoices(ROW_KIND_FIXTURES, {
      needle: "",
      state: "All",
      currency: "EUR",
      row_kind: "ExtNav",
    });
    expect(out.map((r) => r.invoice_id)).toEqual(["rinv_NAV0099"]);
  });
});

// ── canOpenDetail ───────────────────────────────────────────────
//
// S224 / PR-220 — pin the `canOpenDetail` predicate. The chip-
// click handler and the keyboard-Enter handler in
// `InvoiceList.svelte` both consult it; a regression that returns
// `true` for `ExtNav` re-introduces the v2.1.4 404 alert
// (`GET /invoices/rinv_…` has no handler, by design). Per
// CLAUDE.md rule 9 each RowKind variant gets its own assertion
// so a collapse to a constant cannot pass vacuously.
describe("canOpenDetail", () => {
  it("Own rows are openable (canonical invoice has /api/invoices/:id)", () => {
    expect(canOpenDetail("Own")).toBe(true);
  });
  it("ExtNav rows are NOT openable (restored_invoice has no detail GET)", () => {
    expect(canOpenDetail("ExtNav")).toBe(false);
  });
});
