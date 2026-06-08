// S290 / PR-271 — Failure-kind badge classifier for the Pricing tab.
//
// Extracted from `PricingJobsList.svelte` so the four-branch logic can
// be vitest-pinned without component-render tooling. The Svelte
// component renders the returned `{label, className}` shape directly
// into the table cell's failure-kind chip.
//
// Honors [[trust-code-not-operator]]: an operator looking at a Failed
// row should immediately see whether clicking Retry has any chance of
// succeeding — Permanent failures get a RED badge that says "operator
// retry required", Transient gets the existing "auto-retry" copy, and
// the Unknown / legacy `null` cases get a distinct neutral hint so the
// SPA never lies about a row's retry prospects.

export interface FailureKindBadge {
  /** Human-readable HU + EN bilingual label rendered inside the chip. */
  label: string;
  /** Chip CSS class — reuses the existing `chip chip--*` colour vocab so
   *  RED = err, GREEN = ok, AMBER = running, BLUE = queued. */
  className: string;
}

/** Closed-vocab classifier. `null` (legacy PROD_v2.27.[0-5] Failed rows)
 *  and the explicit `"unknown"` verdict share the same neutral badge. */
export function failureKindBadge(
  failureKind: string | null,
): FailureKindBadge | null {
  switch (failureKind) {
    case "permanent":
      return {
        label: "🛑 Operátor művelet szükséges / Operator retry required",
        className: "chip chip--err",
      };
    case "transient":
      return {
        label: "↻ Auto-retry / Átmeneti hiba",
        className: "chip chip--running",
      };
    case "unknown":
    case null:
      return {
        label: "? Ismeretlen / Unknown",
        className: "chip chip--queued",
      };
    default:
      // Defence-in-depth: an unknown failure_kind string from a future
      // backend version. Don't drop silently — surface it verbatim so
      // an operator filing a bug sees the actual value. (CLAUDE.md
      // rule 12 — fail loud.)
      return {
        label: failureKind,
        className: "chip",
      };
  }
}
