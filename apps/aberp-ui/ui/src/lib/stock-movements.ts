// S231 / PR-227 / ADR-0061 — pure helpers for the inventory v1 SPA
// surface. Bilingual reason labels + the manual-form's closed vocab
// (Receipt | Adjustment | Scrap per ADR-0061 §6). Pinned by
// `stock-movements.test.ts`.

import type { StockMovementReason } from "./api";

/** S231 — bilingual operator-facing label for each `MovementReason`.
 * The label ordering matches the SPA dropdown's render order;
 * manual-form-only reasons appear first, upstream-only reasons last
 * (for the ledger display only — the form filters them out). */
export const MOVEMENT_REASON_LABELS: ReadonlyArray<{
  reason: StockMovementReason;
  label_hu: string;
  label_en: string;
  /** Whether the SPA's manual adjustment form offers this reason.
   * Per ADR-0061 §6 only Receipt / Adjustment / Scrap are operator-
   * typed; the other three (BomConsumption / WoCompletion / Dispatch)
   * land via upstream handlers (ADR-0062/0063/0064). */
  manual: boolean;
}> = [
  { reason: "receipt", label_hu: "Bevét", label_en: "Receipt", manual: true },
  {
    reason: "adjustment",
    label_hu: "Korrekció",
    label_en: "Adjustment",
    manual: true,
  },
  { reason: "scrap", label_hu: "Selejt", label_en: "Scrap", manual: true },
  {
    reason: "bom_consumption",
    label_hu: "Anyagfelhasználás (BOM)",
    label_en: "BOM consumption",
    manual: false,
  },
  {
    reason: "wo_completion",
    label_hu: "Készre jelentés (WO)",
    label_en: "WO completion",
    manual: false,
  },
  {
    reason: "dispatch",
    label_hu: "Kiszállítás",
    label_en: "Dispatch",
    manual: false,
  },
];

/** S231 — bilingual label for a reason, falling back to the storage
 * string if a future variant lands without a label. */
export function reasonLabel(reason: StockMovementReason, lang: "hu" | "en" = "en"): string {
  const found = MOVEMENT_REASON_LABELS.find((r) => r.reason === reason);
  if (!found) return reason;
  return lang === "hu" ? found.label_hu : found.label_en;
}

/** S231 — the SPA dropdown shows only the reasons the manual form
 * accepts. The backend refuses the others with 400 (defence in
 * depth), but the dropdown shape stops the operator from picking one
 * by mistake. */
export const MANUAL_REASONS: readonly StockMovementReason[] = MOVEMENT_REASON_LABELS
  .filter((r) => r.manual)
  .map((r) => r.reason);

/** S231 — format a Decimal-as-string qty for display. Strips the
 * DB-side trailing-zero noise (`10.000000` → `10`) but keeps explicit
 * non-zero fractional digits intact (`-3.5` → `-3.5`). Pure string
 * surgery; rust_decimal will re-parse either form unambiguously. */
export function formatQty(qty: string): string {
  if (!qty.includes(".")) return qty;
  const stripped = qty.replace(/0+$/, "");
  return stripped.endsWith(".") ? stripped.slice(0, -1) : stripped;
}

/** S231 — mint a client-side ULID for the `idempotency_key` of a
 * manual adjustment POST. Avoids pulling a `ulid` dep into the SPA;
 * uses crypto.randomUUID() so each call is unique even within the
 * same wall-clock millisecond. Falls back to Math.random() under
 * vitest (jsdom does not implement crypto.randomUUID in every
 * version we test against). */
export function mintIdempotencyKey(): string {
  if (typeof globalThis !== "undefined" && globalThis.crypto?.randomUUID) {
    return `mvt-${globalThis.crypto.randomUUID()}`;
  }
  return `mvt-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
