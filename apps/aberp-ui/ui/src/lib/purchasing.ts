// S440 (ADR-0068) — pure client-side helpers for the Purchasing (PO) module.
//
// Label maps, the PO state-transition graph (MIRRORS the backend's
// authoritative `aberp::purchasing::allowed_transition` — the POST route
// re-validates and is the source of truth), the line-total + PO-total money
// math, the AVL chip decision, and the create-form validator. These give the
// operator instant feedback before the round-trip.
//
// Pinned by `purchasing.test.ts`.

import type {
  ApprovedStatus,
  NewPoLineInput,
  PoLine,
  PoState,
} from "./api";

/** Bilingual (HU / EN) PO state labels. */
export const PO_STATE_LABELS: Record<PoState, string> = {
  draft: "Piszkozat / Draft",
  issued_to_vendor: "Kiadva / Issued",
  partially_received: "Részben beérkezett / Partially received",
  received: "Beérkezett / Received",
  closed: "Lezárva / Closed",
  cancelled: "Visszavonva / Cancelled",
};

/** S440 — the allowed operator-driven next states from a given PO state.
 * MIRRORS `aberp::purchasing::allowed_transition` exactly. The receipt-driven
 * states (`partially_received` / `received`) are NOT operator-set — they come
 * from the receive workflow — so they never appear as transition targets. */
export function allowedNextStates(state: PoState): PoState[] {
  switch (state) {
    case "draft":
      return ["issued_to_vendor", "cancelled"];
    case "issued_to_vendor":
      return ["cancelled"];
    case "partially_received":
      return ["cancelled"];
    case "received":
      return ["closed"];
    case "closed":
    case "cancelled":
      return [];
  }
}

/** S440 — `quantity * unit_price_minor`, overflow-aware (returns null on a
 * non-finite product, mirroring the backend's checked_mul). */
export function lineTotalMinor(
  quantity: number,
  unitPriceMinor: number,
): number | null {
  const v = quantity * unitPriceMinor;
  return Number.isSafeInteger(v) ? v : null;
}

/** S440 — PO money roll-up from the lines + a VAT rate. Mirrors the backend's
 * `vat_minor` (floor) + total. */
export function poTotals(
  lines: { quantity: number; unit_price_minor: number }[],
  vatRatePct: number,
): { subtotalMinor: number; vatMinor: number; totalMinor: number } {
  const subtotalMinor = lines.reduce(
    (acc, l) => acc + (lineTotalMinor(l.quantity, l.unit_price_minor) ?? 0),
    0,
  );
  const vatMinor = Math.floor((subtotalMinor * vatRatePct) / 100);
  return { subtotalMinor, vatMinor, totalMinor: subtotalMinor + vatMinor };
}

/** S440 — format integer minor units + an ISO currency for display. HUF (and
 * other zero-decimal currencies the operator may type) render whole; everything
 * else renders 2 decimals. Self-contained — the NAV `formatTotal` only knows
 * HUF/EUR, but a PO currency can be USD/GBP/… */
const ZERO_DECIMAL_CURRENCIES = new Set(["HUF", "JPY", "KRW"]);

export function formatPoMoney(minor: number, currency: string): string {
  const cur = currency.trim().toUpperCase();
  if (!Number.isFinite(minor)) return `— ${cur}`;
  if (ZERO_DECIMAL_CURRENCIES.has(cur)) {
    return `${minor.toLocaleString("hu-HU")} ${cur}`;
  }
  const sign = minor < 0 ? "-" : "";
  const abs = Math.abs(minor);
  const whole = Math.floor(abs / 100);
  const frac = String(abs % 100).padStart(2, "0");
  return `${sign}${whole.toLocaleString("hu-HU")}.${frac} ${cur}`;
}

/** S440 — the per-line remaining quantity still to receive. */
export function lineRemaining(line: PoLine): number {
  return Math.max(0, line.quantity - line.received_quantity);
}

/** S440 — an AVL chip decision for a PO's vendor-status snapshot
 * ([[trust-code-not-operator]] — the backend already refused Suspended/Revoked
 * at create; this is the operator's visible flag). */
export type AvlChip = {
  tone: "green" | "yellow" | "grey" | "red";
  label: string;
} | null;

export function avlChip(status: ApprovedStatus | null): AvlChip {
  switch (status) {
    case "approved":
      return { tone: "green", label: "AVL: jóváhagyva / approved" };
    case "conditional":
      return { tone: "yellow", label: "AVL: feltételes / conditional" };
    case "pending":
      return { tone: "grey", label: "AVL: függőben / pending" };
    case "suspended":
      return { tone: "red", label: "AVL: felfüggesztve / suspended" };
    case "revoked":
      return { tone: "red", label: "AVL: visszavonva / revoked" };
    case null:
    default:
      return null; // vendor not on the AVL — no chip
  }
}

/** S440 — `true` when this PO's vendor is still Pending AVL approval, so the
 * Issue action must be blocked client-side (the backend re-checks at issue). */
export function issueBlockedByPending(status: ApprovedStatus | null): boolean {
  return status === "pending";
}

/** S440 — create-form validation. Mirrors the backend's loud-rejects so the
 * operator gets inline feedback (the POST re-validates). Returns the list of
 * human-readable problems; empty = ready to submit. */
export function validateNewPo(input: {
  vendor_partner_id: string;
  currency: string;
  vat_rate_pct: number;
  lines: NewPoLineInput[];
}): string[] {
  const errors: string[] = [];
  if (!input.vendor_partner_id.trim()) {
    errors.push("Válasszon beszállítót. / Select a vendor.");
  }
  const cur = input.currency.trim();
  if (cur.length !== 3 || !/^[A-Z]{3}$/.test(cur)) {
    errors.push("A pénznem 3 betűs ISO kód legyen. / Currency must be a 3-letter ISO code.");
  }
  if (input.vat_rate_pct < 0 || input.vat_rate_pct > 100) {
    errors.push("Az ÁFA 0 és 100 között legyen. / VAT must be 0–100.");
  }
  if (input.lines.length === 0) {
    errors.push("Adjon hozzá legalább egy tételt. / Add at least one line.");
  }
  input.lines.forEach((l, i) => {
    if (!l.description.trim()) {
      errors.push(`${i + 1}. tétel: a leírás kötelező. / Line ${i + 1}: description required.`);
    }
    if (!Number.isInteger(l.quantity) || l.quantity <= 0) {
      errors.push(`${i + 1}. tétel: a mennyiség > 0 egész. / Line ${i + 1}: quantity must be a positive integer.`);
    }
    if (!Number.isInteger(l.unit_price_minor) || l.unit_price_minor < 0) {
      errors.push(`${i + 1}. tétel: az egységár ≥ 0. / Line ${i + 1}: unit price must be ≥ 0.`);
    }
  });
  return errors;
}
