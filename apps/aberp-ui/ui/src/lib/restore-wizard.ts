// S180 / PR-180 — NAV-as-DR restore wizard helpers.
//
// Pure functions consumed by `RestoreFromNavWizard.svelte`. Lifted
// into their own module so the load-bearing operator-discipline
// invariants (the "type RESTORE" gate, the year-bounds validator)
// are vitest-pinnable without standing up a Svelte test renderer.
//
// The invariants pinned here:
//
//   - The confirmation gate is EXACT-MATCH on the uppercase token
//     `"RESTORE"`. Lowercase, mixed-case, leading/trailing
//     whitespace, additional characters all fail. The token is NOT
//     localized — the brief calls out "operator-discipline ceremony"
//     and a translated word would weaken the ceremony.
//   - Year bounds mirror the backend's `validate_year`: floor at
//     `MIN_RESTORE_YEAR` (NAV Online Számla went live in 2018),
//     ceiling at the current calendar year. Operator-typed years
//     outside this range surface inline BEFORE the request hits the
//     wire (avoids a wasted 400 round-trip).

/** Mirror of `restore_from_nav_outgoing::MIN_RESTORE_YEAR` (backend
 * constant). Out-of-sync drift surfaces at the integration layer; the
 * `restoreWizardConfirmationToken` + range-test pins catch the SPA
 * side at unit-test time. */
export const MIN_RESTORE_YEAR = 2018;

/** The exact confirmation token the operator must type. Constant
 * (not a translated string) because the ceremony is the point —
 * localizing weakens the operator-discipline signal. */
export const RESTORE_CONFIRMATION_TOKEN = "RESTORE";

/** Closed-vocab discriminator for the wizard's year-input validation
 * outcome. Mirrors the field-error pattern used by partner / seller
 * forms; the SPA renders one inline message per non-Ok variant. */
export type YearValidation =
  | { kind: "ok"; year: number }
  | { kind: "not_integer" }
  | { kind: "below_floor"; floor: number }
  | { kind: "above_ceiling"; ceiling: number };

/** Parse + bounds-check the operator-typed year. Same rules as the
 * backend's `validate_year`. */
export function validateYearInput(
  raw: string,
  currentYear: number,
): YearValidation {
  const trimmed = raw.trim();
  if (trimmed.length === 0) return { kind: "not_integer" };
  // Reject anything that isn't a pure integer string (no decimals,
  // no exponents, no plus-sign prefix).
  if (!/^-?\d+$/.test(trimmed)) return { kind: "not_integer" };
  const parsed = Number.parseInt(trimmed, 10);
  if (Number.isNaN(parsed)) return { kind: "not_integer" };
  if (parsed < MIN_RESTORE_YEAR) {
    return { kind: "below_floor", floor: MIN_RESTORE_YEAR };
  }
  if (parsed > currentYear) {
    return { kind: "above_ceiling", ceiling: currentYear };
  }
  return { kind: "ok", year: parsed };
}

/** EXACT-MATCH check on the confirmation token. The brief calls out
 * uppercase + no localization explicitly. Trimming is INTENTIONALLY
 * absent — pasting `" RESTORE "` should fail because the ceremony's
 * value is in the operator typing it deliberately. */
export function isRestoreConfirmed(input: string): boolean {
  return input === RESTORE_CONFIRMATION_TOKEN;
}

/** Combine the two gates into one "is the submit button enabled"
 * boolean. Used by the Svelte template's `disabled` binding so the
 * test surface is "submit is enabled IFF X" without touching the DOM. */
export function canSubmit(
  yearRaw: string,
  confirmRaw: string,
  currentYear: number,
): boolean {
  const yearOk = validateYearInput(yearRaw, currentYear).kind === "ok";
  return yearOk && isRestoreConfirmed(confirmRaw);
}

/** Format the result summary into a one-line operator-readable
 * string. Used by the Svelte template + by the vitest pin so the
 * format-string is pinned (a future refactor that drops the
 * "errored" count would silently mask failures, so the pin guards
 * against silent omission per CLAUDE.md rule 12). */
export interface RestoreSummary {
  year: number;
  restored: number;
  skipped: number;
  errored: number;
  pages_walked: number;
  elapsed_ms: number;
}

export function formatRestoreSummary(s: RestoreSummary): string {
  return `Year ${s.year}: ${s.restored} restored, ${s.skipped} already present (skipped), ${s.errored} errored. Walked ${s.pages_walked} NAV pages in ${s.elapsed_ms} ms.`;
}
