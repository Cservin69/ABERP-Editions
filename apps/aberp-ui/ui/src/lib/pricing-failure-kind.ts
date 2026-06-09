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
//
// PR-274 / S297 F6: the Permanent verdict splits in two. MarginFloor
// violations are technically Permanent (retry alone won't change the
// answer) but the operator action is NOT "click Retry" — it's "edit
// the margin profile in Quoting Parameters, THEN click Retry". The
// old copy "Operator retry required" misled operators into clicking
// Retry without the configuration edit and watching the row re-fail
// identically. New copy: MarginFloor → "Operator review required";
// everything else Permanent → the original "Operator retry required".

export interface FailureKindBadge {
  /** Human-readable HU + EN bilingual label rendered inside the chip. */
  label: string;
  /** Chip CSS class — reuses the existing `chip chip--*` colour vocab so
   *  RED = err, GREEN = ok, AMBER = running, BLUE = queued. */
  className: string;
}

/** Substring tested against the lowercased `error_reason` to spot the
 *  MarginFloor verdict the engine's `QuoteError::MarginFloorViolation`
 *  emits — same matcher the Rust classifier uses (S290) so the SPA's
 *  copy stays aligned with the backend's classification rule. */
const MARGIN_FLOOR_HINT = "below configured floor";

function isMarginFloorReason(errorReason: string | null | undefined): boolean {
  if (!errorReason) return false;
  return errorReason.toLowerCase().includes(MARGIN_FLOOR_HINT);
}

/** Closed-vocab classifier. `null` (legacy PROD_v2.27.[0-5] Failed rows)
 *  and the explicit `"unknown"` verdict share the same neutral badge.
 *
 *  `errorReason` is optional so callers without the raw reason text
 *  still get the default Permanent copy ("Operator retry required").
 *  When provided AND matching the MarginFloor hint, the badge flips to
 *  "Operator review required" — RED badge, distinct copy. */
export function failureKindBadge(
  failureKind: string | null,
  errorReason?: string | null,
): FailureKindBadge | null {
  switch (failureKind) {
    case "permanent":
      if (isMarginFloorReason(errorReason)) {
        // MarginFloor: operator must adjust the margin profile in
        // Quoting Parameters BEFORE clicking Retry. Retry alone fails
        // identically. Distinct copy keeps the badge truthful.
        return {
          label: "🛑 Operátor felülvizsgálat szükséges / Operator review required",
          className: "chip chip--err",
        };
      }
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
