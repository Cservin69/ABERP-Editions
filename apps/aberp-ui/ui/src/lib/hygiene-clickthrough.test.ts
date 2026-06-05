// PR-223 / S227 — vitest pins for the StatisticsPage → InvoiceList
// click-through machinery.
//
// Three concerns to pin:
//
//   1. `clickTargetForFlag` table — every hygiene flag maps to its
//      brief-specified URL, or `null` for the AR-side past-deadline
//      flag we cannot deliver an exact filter for without a backend
//      wire-shape extension (out-of-scope per the S227 brief).
//   2. `parseInvoicesUrl` round-trip — every URL `clickTargetForFlag`
//      emits parses back to a non-empty `InvoicesUrlInit` with the
//      original facets set. A round-trip-by-construction pin guards
//      against a future param-name drift on either side.
//   3. Closed-vocab discards — a URL with hand-typed garbage falls
//      back to `EMPTY_URL_INIT` rather than crashing the consumer
//      (CLAUDE.md rule 7).
//
// Additionally, the outgoing-hygiene filter predicate (`pending`,
// `no_partner`) is pinned via `filterInvoices` so a regression that
// collapses the gate or mismatches the SPA state list to the report's
// `CountedKind::PendingDraft` classification surfaces here, not in
// production when the dashboard count diverges from the list row
// count.

import { describe, expect, it } from "vitest";

import type { InvoiceListItem } from "./api";
import {
  EMPTY_URL_INIT,
  PENDING_STATES,
  clickTargetForFlag,
  parseInvoicesUrl,
  type HygieneFlag,
} from "./hygiene-clickthrough";
import { filterInvoices, type InvoiceFilterSpec } from "./invoice-list";

// ──────────────────────────────────────────────────────────────────────
// clickTargetForFlag — mapping table
// ──────────────────────────────────────────────────────────────────────

describe("clickTargetForFlag — every flag maps to brief-specified URL", () => {
  // Per-flag pin, NOT a single table-driven loop, so a regression
  // collapsing one arm to a constant cannot pass vacuously (CLAUDE.md
  // rule 9). Each `it` block names the dashboard row it covers.

  it("outgoing_pending → tab=outgoing & hygiene=pending", () => {
    expect(clickTargetForFlag("outgoing_pending")).toEqual({
      hash: "#/invoices?tab=outgoing&hygiene=pending",
    });
  });

  it("outgoing_rejected → tab=outgoing & state=Rejected", () => {
    expect(clickTargetForFlag("outgoing_rejected")).toEqual({
      hash: "#/invoices?tab=outgoing&state=Rejected",
    });
  });

  it("outgoing_abandoned → tab=outgoing & state=Abandoned", () => {
    expect(clickTargetForFlag("outgoing_abandoned")).toEqual({
      hash: "#/invoices?tab=outgoing&state=Abandoned",
    });
  });

  it("restored_no_partner → tab=outgoing & kind=ExtNav & hygiene=no_partner", () => {
    expect(clickTargetForFlag("restored_no_partner")).toEqual({
      hash: "#/invoices?tab=outgoing&kind=ExtNav&hygiene=no_partner",
    });
  });

  it("outstanding_past_deadline → null (no row field; static)", () => {
    // The AR-side past-deadline flag is the one entry in this table
    // intentionally non-clickable: `InvoiceListItem` does not carry
    // `payment_deadline`, and adding it is a backend change the S227
    // brief lists as out-of-scope. A regression that emits a URL here
    // would silently land the operator on a SUPERSET of the
    // dashboard's count (every unpaid Finalized invoice, not just
    // the deadline-overdue subset) — the rule violates
    // [[hulye-biztos]]. Pin returns `null` so the dashboard renders
    // the row static.
    expect(clickTargetForFlag("outstanding_past_deadline")).toBeNull();
  });

  it("payable_past_deadline → tab=incoming & hygiene=past_deadline", () => {
    // The AP-side counterpart IS clickable because `IncomingInvoice`
    // already carries `payment_deadline` (PR-179 wire shape) — no
    // backend touch required.
    expect(clickTargetForFlag("payable_past_deadline")).toEqual({
      hash: "#/invoices?tab=incoming&hygiene=past_deadline",
    });
  });

  it("storno_chain → tab=outgoing & state=Storno", () => {
    expect(clickTargetForFlag("storno_chain")).toEqual({
      hash: "#/invoices?tab=outgoing&state=Storno",
    });
  });

  it("modification_chain → tab=outgoing & state=Amended", () => {
    expect(clickTargetForFlag("modification_chain")).toEqual({
      hash: "#/invoices?tab=outgoing&state=Amended",
    });
  });
});

// ──────────────────────────────────────────────────────────────────────
// parseInvoicesUrl — round-trip + edge cases
// ──────────────────────────────────────────────────────────────────────

describe("parseInvoicesUrl — round-trip from clickTargetForFlag URLs", () => {
  const ROUND_TRIPS: ReadonlyArray<{ flag: HygieneFlag; expect: object }> = [
    {
      flag: "outgoing_pending",
      expect: { tab: "outgoing", outgoing: { hygiene: "pending" } },
    },
    {
      flag: "outgoing_rejected",
      expect: { tab: "outgoing", outgoing: { state: "Rejected" } },
    },
    {
      flag: "outgoing_abandoned",
      expect: { tab: "outgoing", outgoing: { state: "Abandoned" } },
    },
    {
      flag: "restored_no_partner",
      expect: {
        tab: "outgoing",
        outgoing: { row_kind: "ExtNav", hygiene: "no_partner" },
      },
    },
    {
      flag: "payable_past_deadline",
      expect: { tab: "incoming", incoming: { hygiene: "past_deadline" } },
    },
    {
      flag: "storno_chain",
      expect: { tab: "outgoing", outgoing: { state: "Storno" } },
    },
    {
      flag: "modification_chain",
      expect: { tab: "outgoing", outgoing: { state: "Amended" } },
    },
  ];

  for (const { flag, expect: expected } of ROUND_TRIPS) {
    it(`${flag} URL parses back to its facet init`, () => {
      const target = clickTargetForFlag(flag);
      expect(target).not.toBeNull();
      const parsed = parseInvoicesUrl(target!.hash);
      expect(parsed.hasInit).toBe(true);
      expect(parsed).toMatchObject(expected);
    });
  }
});

describe("parseInvoicesUrl — closed-vocab discard + edge cases", () => {
  it("returns EMPTY_URL_INIT for a hash with no query string", () => {
    const parsed = parseInvoicesUrl("#/invoices");
    expect(parsed).toEqual(EMPTY_URL_INIT);
    expect(parsed.hasInit).toBe(false);
  });

  it("returns EMPTY_URL_INIT for a hash naming a different route", () => {
    // A `?tab=outgoing` on a non-invoices route MUST NOT bleed init
    // into the invoices list when the operator later navigates back
    // — the parser refuses to honour foreign-route query strings.
    expect(parseInvoicesUrl("#/statistics?tab=outgoing")).toEqual(
      EMPTY_URL_INIT,
    );
  });

  it("silently discards unknown state vocab without throwing", () => {
    const parsed = parseInvoicesUrl("#/invoices?state=Archived");
    expect(parsed.outgoing.state).toBeUndefined();
    expect(parsed.hasInit).toBe(false);
  });

  it("silently discards unknown row_kind vocab", () => {
    const parsed = parseInvoicesUrl("#/invoices?kind=ThirdKind");
    expect(parsed.outgoing.row_kind).toBeUndefined();
  });

  it("silently discards unknown hygiene vocab", () => {
    const parsed = parseInvoicesUrl("#/invoices?hygiene=lateToTheParty");
    expect(parsed.outgoing.hygiene).toBeUndefined();
    expect(parsed.incoming.hygiene).toBeUndefined();
  });

  it("accepts a bare query string with no leading # or slug", () => {
    // Tolerant parser for callers that pass `window.location.search`
    // or compose a query string directly.
    const parsed = parseInvoicesUrl("tab=incoming&hygiene=past_deadline");
    expect(parsed.tab).toBe("incoming");
    expect(parsed.incoming.hygiene).toBe("past_deadline");
  });

  it("first key wins on a duplicate param (defence vs hand-typed URL)", () => {
    const parsed = parseInvoicesUrl(
      "#/invoices?tab=outgoing&tab=incoming&state=Rejected",
    );
    expect(parsed.tab).toBe("outgoing");
    expect(parsed.outgoing.state).toBe("Rejected");
  });

  it("survives URL-encoded values (operator copy-paste)", () => {
    // `Rejected` doesn't need encoding, but the parser should accept
    // an encoded form just in case a future facet value carries a
    // non-ASCII character (Hungarian label, etc.).
    const parsed = parseInvoicesUrl("#/invoices?state=%52ejected");
    expect(parsed.outgoing.state).toBe("Rejected");
  });
});

// ──────────────────────────────────────────────────────────────────────
// Outgoing hygiene filter predicate
// ──────────────────────────────────────────────────────────────────────

// Helper — build a minimal `InvoiceListItem` for the filter test. We
// only need the fields the gate inspects; everything else gets a
// placeholder.
function frow(
  overrides: Partial<InvoiceListItem> & {
    invoice_id: string;
    state: InvoiceListItem["state"];
  },
): InvoiceListItem {
  return {
    invoice_id: overrides.invoice_id,
    sequence_number: 1,
    fiscal_year: 2026,
    state: overrides.state,
    total_gross: 100,
    has_chain_children: false,
    is_storno: false,
    currency: "HUF",
    buyer_name: overrides.buyer_name ?? "Alpha Kft.",
    payment: null,
    bank_account: null,
    row_kind: overrides.row_kind ?? "Own",
    source_nav_invoice_number: overrides.source_nav_invoice_number ?? null,
    issue_date: overrides.issue_date ?? null,
    ...overrides,
  };
}

describe("filterInvoices — hygiene=pending mirrors CountedKind::PendingDraft", () => {
  // The dashboard's `outgoing_pending_count` counts invoices in the
  // PendingDraft classification. Per `reports::ReportTrace::classify`
  // (reports.rs::~L504), PendingDraft strictly excludes
  // `Submitted` / `Recovered` (those bucket as `Counted`) — even
  // though both states are pre-final from the UI's lifecycle ordering.
  // The pin enforces the SPA mirrors this distinction.

  it("PENDING_STATES table is exactly {Ready, Pending, PendingNavExists}", () => {
    expect([...PENDING_STATES].sort()).toEqual(
      ["Pending", "PendingNavExists", "Ready"].sort(),
    );
  });

  it("hygiene=pending admits Ready / Pending / PendingNavExists rows", () => {
    const rows = [
      frow({ invoice_id: "R", state: "Ready" }),
      frow({ invoice_id: "P", state: "Pending" }),
      frow({ invoice_id: "X", state: "PendingNavExists" }),
    ];
    const spec: InvoiceFilterSpec = {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "All",
      hygiene: "pending",
    };
    expect(filterInvoices(rows, spec).map((r) => r.invoice_id)).toEqual([
      "R",
      "P",
      "X",
    ]);
  });

  it("hygiene=pending REJECTS Submitted / Recovered (Counted, not PendingDraft)", () => {
    // Counter-pin per CLAUDE.md rule 9: a regression that widens
    // `pending` to "every pre-final state" would also let `Submitted`
    // and `Recovered` rows pass; that would diverge from the
    // dashboard's count by exactly the in-flight set.
    const rows = [
      frow({ invoice_id: "S", state: "Submitted" }),
      frow({ invoice_id: "C", state: "Recovered" }),
    ];
    const spec: InvoiceFilterSpec = {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "All",
      hygiene: "pending",
    };
    expect(filterInvoices(rows, spec)).toEqual([]);
  });

  it("hygiene=pending REJECTS terminal states (Finalized / Rejected / Storno / Amended / Abandoned)", () => {
    const rows = [
      frow({ invoice_id: "F", state: "Finalized" }),
      frow({ invoice_id: "J", state: "Rejected" }),
      frow({ invoice_id: "S", state: "Storno" }),
      frow({ invoice_id: "M", state: "Amended" }),
      frow({ invoice_id: "A", state: "Abandoned" }),
    ];
    const spec: InvoiceFilterSpec = {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "All",
      hygiene: "pending",
    };
    expect(filterInvoices(rows, spec)).toEqual([]);
  });
});

describe("filterInvoices — hygiene=no_partner matches restored_no_partner_count", () => {
  it("admits rows with null or whitespace-only buyer_name", () => {
    const rows = [
      frow({
        invoice_id: "N",
        state: "Finalized",
        row_kind: "ExtNav",
        buyer_name: null,
      }),
      frow({
        invoice_id: "W",
        state: "Finalized",
        row_kind: "ExtNav",
        buyer_name: "   ",
      }),
    ];
    const spec: InvoiceFilterSpec = {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "All",
      hygiene: "no_partner",
    };
    expect(filterInvoices(rows, spec).map((r) => r.invoice_id)).toEqual([
      "N",
      "W",
    ]);
  });

  it("REJECTS rows with a non-empty buyer_name", () => {
    const rows = [
      frow({
        invoice_id: "K",
        state: "Finalized",
        row_kind: "ExtNav",
        buyer_name: "Acme Kft.",
      }),
    ];
    const spec: InvoiceFilterSpec = {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "All",
      hygiene: "no_partner",
    };
    expect(filterInvoices(rows, spec)).toEqual([]);
  });

  it("ANDs with the row_kind=ExtNav facet (operator dashboard intent)", () => {
    // The dashboard's `restored_no_partner_count` only counts
    // `restored_invoice` rows. The click-through emits BOTH
    // `kind=ExtNav` AND `hygiene=no_partner` so the URL fully
    // constrains the resulting list to that population.
    const rows = [
      frow({
        invoice_id: "Own-empty",
        state: "Ready",
        row_kind: "Own",
        buyer_name: null,
      }),
      frow({
        invoice_id: "ExtNav-empty",
        state: "Finalized",
        row_kind: "ExtNav",
        buyer_name: null,
      }),
    ];
    const spec: InvoiceFilterSpec = {
      needle: "",
      state: "All",
      currency: "All",
      row_kind: "ExtNav",
      hygiene: "no_partner",
    };
    expect(filterInvoices(rows, spec).map((r) => r.invoice_id)).toEqual([
      "ExtNav-empty",
    ]);
  });
});
