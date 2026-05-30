// PR-179 / session-179 — pure helpers for the IncomingInvoiceList
// rendering. Closed-vocab status mapping mirrors the backend's
// `IncomingInvoiceStatus` (`Outstanding` / `Paid` / `Irrelevant`).
// Failure to recognise a value falls back to a muted "?" chip rather
// than crashing — same loud-but-not-fatal posture as the AR-side
// `labelMeta` in `./labels.ts`. The pin in
// `incoming-invoice-status.test.ts` enforces every status maps to a
// CSS class + glyph; an additional fixture pins the bilingual
// HU/EN labels.

export type IncomingInvoiceStatus = "Outstanding" | "Paid" | "Irrelevant" | string;

export interface StatusMeta {
  /** CSS class suffix the chip applies (`status-chip--outstanding`,
   * etc). The component reads this directly into a class:directive. */
  cssClass: string;
  /** Single-glyph Unicode mark (no icon library by design). */
  glyph: string;
  /** Hungarian primary label (matches the rest of the HU-primary SPA). */
  label_hu: string;
  /** English secondary label rendered as a sub-line under the HU. */
  label_en: string;
}

export const STATUS_META: Record<"Outstanding" | "Paid" | "Irrelevant", StatusMeta> = {
  Outstanding: {
    cssClass: "outstanding",
    glyph: "⌛",
    label_hu: "Kifizetésre vár",
    label_en: "Outstanding",
  },
  Paid: {
    cssClass: "paid",
    glyph: "✓",
    label_hu: "Kifizetve",
    label_en: "Paid",
  },
  Irrelevant: {
    cssClass: "irrelevant",
    glyph: "−",
    label_hu: "Nem releváns",
    label_en: "Irrelevant",
  },
};

const UNKNOWN_META: StatusMeta = {
  cssClass: "unknown",
  glyph: "?",
  label_hu: "Ismeretlen",
  label_en: "Unknown",
};

export function metaForStatus(status: string): StatusMeta {
  if (status === "Outstanding" || status === "Paid" || status === "Irrelevant") {
    return STATUS_META[status];
  }
  return UNKNOWN_META;
}

/** The three valid TRANSITION targets the SPA can offer from a given
 * current status. Mirrors the backend's strict-transition graph
 * (`Outstanding → Paid | Irrelevant`; `Paid → Outstanding`;
 * `Irrelevant → Outstanding`; `Paid ↔ Irrelevant` blocked — operator
 * must clear to Outstanding first). The component uses this to decide
 * which action buttons to render per row. */
export type ActionTarget = "mark-paid" | "mark-outstanding" | "mark-irrelevant";

export function actionsForStatus(status: string): ActionTarget[] {
  switch (status) {
    case "Outstanding":
      return ["mark-paid", "mark-irrelevant"];
    case "Paid":
      return ["mark-outstanding"];
    case "Irrelevant":
      return ["mark-outstanding"];
    default:
      return [];
  }
}
