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

// S347 / PR-39 (F1+F2) — granular priced-writeback transport verdicts.
// The Rust `WritebackOutcome` prefixes every post-stage failure reason
// with a stable `writeback:<tag>` token (see
// `apps/aberp/src/quote_pricing_pipeline.rs`). When the SPA sees that
// token it swaps the coarse `transient`/`permanent` chip for the precise
// bilingual operator copy — so a CDN misroute reads "Routing
// misconfigured", not the old `? Ismeretlen / Unknown`. Labels mirror
// `WritebackOutcome::label_hu` / `label_en`; the className mirrors the
// retryable split (`chip--running` = retryable, `chip--err` = operator
// must act). Keep this map in sync with the Rust enum.
const WRITEBACK_BADGES: Record<string, FailureKindBadge> = {
  routing_misconfigured: {
    label: "🛑 Útvonal-hiba / Routing misconfigured",
    className: "chip chip--err",
  },
  unauthorized: {
    label: "🛑 Hitelesítési hiba / Unauthorized",
    className: "chip chip--err",
  },
  forbidden: {
    label: "🛑 Hozzáférés megtagadva / Forbidden",
    className: "chip chip--err",
  },
  non_json_response: {
    label: "🛑 Nem-JSON válasz / Non-JSON response",
    className: "chip chip--err",
  },
  malformed_app_response: {
    label: "🛑 Hibás válasz-szerkezet / Malformed app response",
    className: "chip chip--err",
  },
  app_rejected: {
    label: "🛑 Storefront elutasította / Storefront rejected",
    className: "chip chip--err",
  },
  app_errored: {
    label: "↻ Storefront szerverhiba / Storefront server error",
    className: "chip chip--running",
  },
  timeout: {
    label: "↻ Időtúllépés / Timeout",
    className: "chip chip--running",
  },
  transport_error: {
    label: "↻ Hálózati hiba / Transport error",
    className: "chip chip--running",
  },
};

/** S349 / PR-40 (U1) — resolve a `quote.priced_writeback_outcome`
 *  closed-vocab `outcome` tag (as stored in the audit payload) to its
 *  bilingual badge. Reuses [`WRITEBACK_BADGES`] for the failure tags and
 *  adds the `success` case the failure-only error-reason path never
 *  sees. Used by the detail panel's "Last writeback outcome" section to
 *  render the structured verdict (the table cell uses `failureKindBadge`
 *  off the free-text reason instead). An unrecognised tag from a future
 *  backend surfaces verbatim rather than being dropped (CLAUDE.md #12). */
export function writebackOutcomeBadge(outcome: string): FailureKindBadge {
  if (outcome === "success") {
    return { label: "✓ Sikeres / Success", className: "chip chip--ok" };
  }
  return WRITEBACK_BADGES[outcome] ?? { label: outcome, className: "chip" };
}

/** Pull the `writeback:<tag>` token out of an error reason and resolve it
 *  to the granular badge. `null` when the reason is not a typed
 *  priced-writeback verdict (legacy rows, non-post-stage failures). */
function writebackBadge(
  errorReason: string | null | undefined,
): FailureKindBadge | null {
  if (!errorReason) return null;
  const match = /(?:^|\s)writeback:([a-z_]+)/.exec(errorReason.toLowerCase());
  if (!match) return null;
  return WRITEBACK_BADGES[match[1]] ?? null;
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
  // S347 / PR-39 — a typed priced-writeback verdict wins over the coarse
  // failure_kind chip: the operator needs "Routing misconfigured", not the
  // generic "Operator retry required". Non-writeback reasons (engine
  // permanent failures, etc.) fall through to the switch below unchanged.
  const writeback = writebackBadge(errorReason);
  if (writeback) return writeback;

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
