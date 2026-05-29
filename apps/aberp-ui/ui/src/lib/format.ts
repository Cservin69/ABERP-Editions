// PR-44ε / session-53 — currency-aware formatters for the operator
// surface, per ADR-0037 §1.a + §1.c.
//
// Pre-PR-44ε the only formatter on the SPA was the inline
// `Intl.NumberFormat("hu-HU", {style: "currency", currency: "HUF"})`
// in InvoiceList.svelte / InvoiceDetail.svelte (one copy each, byte-
// identical). PR-44γ stamped the EUR-cents interpretation on
// `total_gross` for non-HUF invoices, so the per-row formatter now
// needs the currency tag to pick the symbol AND the minor-unit
// divisor — without it, an EUR invoice's cents render as forints
// (off by 100× plus wrong symbol).
//
// Module-level rather than per-component for two reasons:
//
// 1. **Single source of truth.** The HUF formatter exists in two
//    Svelte files pre-PR-44ε; adding the EUR + rate-metadata
//    formatters in each file would triple the duplication and
//    every per-formatter tweak would need a search-and-replace.
//    Module-level matches `labels.ts`'s posture for per-label
//    affordances — affordance shape lives in one file, components
//    import.
// 2. **Testability.** Vitest pins the four formatters at gate
//    time (`format.test.ts`); inline formatters in Svelte files
//    are reachable from the component-render path only, which we
//    cannot exercise from vitest without a Svelte 5 test runner
//    setup (deferred per CLAUDE.md rule 2 — minimum code).
//
// Naming convention: every export is a small pure function with
// no implicit defaults — `formatTotal(value, currency)` makes the
// currency dependency syntactic, so a regression that drops the
// `currency` argument at a call site is a TS error (`Argument of
// type ... is not assignable`) rather than a silent
// misinterpretation.

import type { Currency } from "./api";

// Hungarian conventions per the reference invoice template
// (see agent memory `reference_aberp_invoice_template.md`):
//   - HUF: no fractional part, space-separated thousands, trailing
//     " Ft" suffix (e.g. `654 883 Ft`). `Intl.NumberFormat`'s
//     `style: "currency", currency: "HUF"` produces exactly this
//     shape under the `hu-HU` locale.
//   - EUR: two fractional digits, decimal comma (Hungarian
//     convention), thin-space thousands, leading `€` symbol
//     (e.g. `€8 636,00`). `Intl.NumberFormat`'s
//     `style: "currency", currency: "EUR"` under `hu-HU` produces
//     this shape; whether the symbol leads or trails depends on
//     the runtime ICU data, but on every modern browser/Node it
//     leads for EUR under `hu-HU`.
const HUF_FORMATTER = new Intl.NumberFormat("hu-HU", {
  style: "currency",
  currency: "HUF",
  minimumFractionDigits: 0,
  maximumFractionDigits: 0,
  useGrouping: true,
});

// `currencyDisplay: "narrowSymbol"` forces the `€` glyph in place of
// the ICU-default `EUR` ISO-code suffix that some Node / browser
// builds emit under `hu-HU` (verified empirically on Node 20 ICU —
// the default `"symbol"` falls back to the ISO code for EUR but
// `"narrowSymbol"` resolves to `€`). The printed-invoice reference
// template uses the `€` glyph, so the SPA matches.
//
// `useGrouping: true` is explicit because some ICU builds drop the
// thousand separator for EUR under `hu-HU` when the option is left
// at its default (also verified empirically on Node 20 — the
// default produced `"8636,00 €"` rather than `"8 636,00 €"`).
const EUR_FORMATTER = new Intl.NumberFormat("hu-HU", {
  style: "currency",
  currency: "EUR",
  currencyDisplay: "narrowSymbol",
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
  useGrouping: true,
});

/** Format an invoice's `total_gross` for the operator surface.
 *
 * The minor-unit interpretation depends on `currency`:
 *   - `"HUF"` — `value` is whole forints (HUF has no sub-unit per
 *     ADR-0009 §1 / `Huf(pub i64)`); rendered as `"654 883 Ft"`.
 *   - `"EUR"` — `value` is EUR cents (the PR-44γ posture stores
 *     EUR amounts in the underlying `i64` as cents); divided by
 *     100 before formatting and rendered as `"€8 636,00"`.
 *
 * `null` renders as the em-dash `"—"` per the existing list-row
 * + detail-modal posture for unset totals.
 */
export function formatTotal(value: number | null, currency: Currency): string {
  if (value === null) return "—";
  if (currency === "EUR") {
    return EUR_FORMATTER.format(value / 100);
  }
  return HUF_FORMATTER.format(value);
}

/** ADR-0049 §Screen render (session 156) — format an invoice total,
 * negating the sign when the invoice IS a storno.
 *
 * The billing tables store a storno's `total_gross` POSITIVE (the
 * negation lives only in the NAV-XML render path, which the
 * buyer-facing PDF parses). The operator's mental model — and the PDF —
 * show a storno as a negative document (`-127 000 Ft`). This helper
 * flips the sign for display only; the wire value stays positive
 * (audit-immutable). `Intl.NumberFormat`'s currency style renders the
 * minus in the Hungarian-correct position, so we just negate the number
 * and delegate to [`formatTotal`]. `null` passes through as the em-dash.
 */
export function formatInvoiceTotal(
  value: number | null,
  currency: Currency,
  isStorno: boolean,
): string {
  if (value === null) return formatTotal(null, currency);
  return formatTotal(isStorno ? -value : value, currency);
}

/** Format the MNB exchange rate for the operator surface.
 *
 * Normalises to 6 decimal places per ADR-0037 §1.c / C11 — the
 * backend serialises at exactly 6 decimals (the audit-ledger
 * stamp + the NAV body's `<exchangeRate>` field both pin this
 * shape), but a future drift that drops decimals on the wire
 * would render with fewer here unless we re-format. A
 * non-parseable input passes through unchanged so a malformed
 * value is operator-visible rather than silently zeroed per
 * CLAUDE.md rule 12.
 */
export function formatRate(rate: string): string {
  const n = Number(rate);
  if (!Number.isFinite(n)) return rate;
  return n.toFixed(6);
}

/** Format an HUF-equivalent amount per the Hungarian convention.
 *
 * Reuses [`formatTotal`] with the HUF branch — the per-VAT-rate
 * HUF amount and the gross HUF-equivalent on the printed-invoice
 * reference template use the same `"654 883 Ft"` shape.
 */
export function formatHufEquivalent(value: number): string {
  return HUF_FORMATTER.format(value);
}

/** Format an MNB-rate publication date for the operator surface.
 *
 * Pass-through — the backend emits ISO-8601 `YYYY-MM-DD`
 * (`OffsetDateTime::format` + `format_description!("[year]-[month]-[day]")`
 * at PR-44γ), which is what the operator surface displays. A
 * future PR that wants a Hungarian-locale render (e.g.,
 * `2026. 05. 22.`) can extend this helper additively without
 * touching the call sites per CLAUDE.md rule 3.
 */
export function formatRateDate(date: string): string {
  return date;
}

/** PR-99 Item 5 — format an ISO-8601 `YYYY-MM-DD` date string in the
 * Hungarian locale display form `YYYY. MM. DD.` used on the printed
 * PDF (the operator's reference surface). A malformed input passes
 * through verbatim so a backend drift surfaces visibly per
 * CLAUDE.md rule 12. `null` renders as the em-dash placeholder, same
 * posture as `formatTotal(null, _)`. */
export function formatInvoiceDate(date: string | null): string {
  if (date === null) return "—";
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(date);
  if (!match) return date;
  return `${match[1]}. ${match[2]}. ${match[3]}.`;
}

/** PR-44ε.UI / session-58 — build the browser-side download filename
 * for the printed-invoice PDF. The Rust side emits the same shape on
 * the `Content-Disposition` header (`serve::pdf_filename_for_invoice`),
 * but the SPA cannot read that header through Tauri's `invoke`
 * boundary; the SPA composes the filename locally for the synthetic
 * `<a download>` click instead. Both sides emit
 * `invoice_<invoice_number>.pdf` verbatim — pinned at the Rust side
 * by `pdf_filename_uses_invoice_number` and at the SPA side by the
 * vitest in `format.test.ts`.
 */
export function filenameForInvoice(invoiceNumber: string): string {
  return `invoice_${invoiceNumber}.pdf`;
}

/** PR-88 / session-113 — minor-unit count per currency. EUR is a
 * 2-decimal currency (1 EUR = 100 cents); HUF is 0-decimal (1 HUF =
 * 1 forint, no sub-unit per ADR-0009 §1 / `Huf(pub i64)`). The
 * parser uses this to validate operator-typed decimal precision and
 * to scale whole-major-unit input to minor units. */
const MINOR_DECIMALS: Record<Currency, number> = {
  HUF: 0,
  EUR: 2,
};

/** PR-88 / session-113 — parse an operator-typed money amount string
 * into the integer minor-unit count the wire shape carries.
 *
 * **The bug this closes**: pre-PR-88 the IssueInvoice form bound the
 * operator's typed value DIRECTLY to a `unitPriceMinor: number`
 * field on the form state, and the composer sent that integer on
 * the wire verbatim. For HUF this happened to work (HUF is
 * 0-decimal, so `340` typed = 340 forints on the wire). For EUR
 * (2-decimal) the same `340` typed became 340 minor units = 3.40
 * EUR on the wire — a 100× underbill. Ervin issued one invoice at
 * 1/100 of intent before catching it.
 *
 * **Rules** (exhaustively pinned in `format.test.ts`):
 *   - A bare integer is interpreted as WHOLE MAJOR UNITS. `340` →
 *     340.00 EUR (= 34000 cents); never as 3.40 EUR.
 *   - `.` and `,` are both accepted as the decimal separator
 *     (Hungarian uses comma; the operator's keyboard convenience
 *     wins over locale orthodoxy).
 *   - ASCII spaces and NBSP are stripped as thousands separators
 *     (`340 000` → 340000 major units).
 *   - Cents (sub-unit) is only ever produced when the operator
 *     EXPLICITLY types a separator. Never auto-derived.
 *   - The fractional part may not exceed the currency's decimals
 *     (HUF rejects any decimal; EUR rejects 3+ decimal digits).
 *   - Negative, malformed, or empty input returns `null`. The form's
 *     `required` attribute + the backend preflight's
 *     `LineItemUnitPriceNonPositive` gate already cover the empty-
 *     after-trim path; the parser refuses to guess.
 *
 * Pure function — no DOM, no side effects — so vitest can pin every
 * row of the rule table without mounting a Svelte component.
 *
 * Returns `null` for unparseable input. The composer treats `null`
 * as 0 on the wire so the existing preflight surfaces the inline
 * error (see `composeIssueInvoiceBody`).
 */
export function parseAmountToMinor(raw: string, currency: Currency): number | null {
  if (typeof raw !== "string") return null;
  const trimmed = raw.trim();
  if (trimmed === "") return null;

  // Strip ASCII spaces + NBSP as thousands separators. Hungarian
  // writes `340 000` with a regular space; some locales paste with
  // NBSP. Both collapse to bare digits.
  const noSpaces = trimmed.replace(/[\s ]/g, "");

  // Closed grammar: one-or-more digits, optionally followed by ONE
  // decimal separator (`.` or `,`) + one-or-more digits. No leading
  // sign (invoice unit prices are positive — storno/modification is
  // a separate flow per the backend preflight's
  // `LineItemUnitPriceNonPositive`). No trailing separator. No
  // bare-decimal `.50`.
  const match = /^(\d+)(?:[.,](\d+))?$/.exec(noSpaces);
  if (!match) return null;

  const wholePart = match[1];
  const fracPart = match[2] ?? "";
  const decimals = MINOR_DECIMALS[currency];

  // Reject more decimal digits than the currency supports. `340,505`
  // for EUR is operator ambiguity (rounded to the half-cent?
  // truncated?); refuse rather than guess. HUF with ANY fractional
  // part rejects here.
  if (fracPart.length > decimals) return null;

  // Pad the fractional part with trailing zeros so `340.5` (EUR) →
  // "50" (= 50 cents), not "5" (= 5 cents). For HUF (decimals=0)
  // this is a no-op.
  const fracPadded = fracPart.padEnd(decimals, "0");

  // Compose minor units by string concatenation then integer parse —
  // avoids float arithmetic (`3.40 * 100 = 339.99999...`) entirely.
  // `parseInt("099", 10)` correctly returns 99 (no octal coercion in
  // modern JS).
  const combined = wholePart + fracPadded;
  const minor = parseInt(combined, 10);
  if (!Number.isSafeInteger(minor)) return null;
  return minor;
}

/** PR-88 / session-113 — inverse of [`parseAmountToMinor`]. Formats an
 * integer minor-unit count back into the operator-editable input
 * string the modification form pre-fills with. Round-trips: for any
 * valid `parseAmountToMinor(s, c)` returning `n`,
 * `formatMinorToInput(n, c)` returns a canonical form that re-parses
 * to the same `n`.
 *
 * Uses `.` as the decimal separator (the parser accepts both `.` and
 * `,` — the operator can re-type with comma if they prefer the
 * Hungarian convention). For HUF (0-decimal) the output is the bare
 * integer with no separator. */
export function formatMinorToInput(minor: number, currency: Currency): string {
  if (!Number.isFinite(minor) || !Number.isInteger(minor)) return "";
  const decimals = MINOR_DECIMALS[currency];
  if (decimals === 0) return String(minor);
  const sign = minor < 0 ? "-" : "";
  const abs = Math.abs(minor);
  const divisor = 10 ** decimals;
  const whole = Math.floor(abs / divisor);
  const frac = abs % divisor;
  const fracStr = String(frac).padStart(decimals, "0");
  return `${sign}${whole}.${fracStr}`;
}

/** S157 — parse an operator-typed line quantity into the canonical
 * dot-decimal string the wire carries (e.g. `"1.5"`).
 *
 * **The bug this closes**: pre-S157 the IssueInvoice line quantity was an
 * `<input type="number" step="1">` bound to a `number`, so the operator
 * could only enter whole units — `1.5` consulting days was unreachable.
 *
 * **Rules** (exhaustively pinned in `format.test.ts`):
 *   - `.` and `,` are both accepted as the decimal separator (Hungarian
 *     writes `1,5`; the operator's keyboard often types `1.5`). Both
 *     decode to the same value.
 *   - ASCII spaces + NBSP are stripped (paste tolerance).
 *   - The value must be strictly positive; `0`, negatives, blank, and
 *     non-numeric all return `null`.
 *   - At most 6 fractional digits — NAV's `<quantity>` ceiling and the
 *     `DECIMAL(18,6)` storage scale. More digits return `null` rather
 *     than silently rounding (CLAUDE.md rule 12 — fail loud).
 *
 * Returns the canonical **string** (not a number) so the wire stays exact
 * — the C11 Decimal-as-string convention the `exchange_rate` field
 * already uses. The composer sends `"0"` on `null` so the backend
 * preflight's `LineItemQuantityZero` renders the inline error rather than
 * a silent bad-quantity issuance. Pure function — no DOM — so vitest pins
 * every rule-table row without mounting a Svelte component.
 */
export function parseDecimalQuantity(raw: string): string | null {
  if (typeof raw !== "string") return null;
  const noSpaces = raw.trim().replace(/[\s ]/g, "");
  if (noSpaces === "") return null;
  // One-or-more digits, optionally a single `.`/`,` separator + 1..6
  // fractional digits. No sign (quantities are positive), no bare-decimal
  // `.5`, no trailing separator.
  const match = /^(\d+)(?:[.,](\d{1,6}))?$/.exec(noSpaces);
  if (!match) return null;
  const whole = match[1];
  const frac = match[2] ?? "";
  // Canonical dot-decimal; trim trailing-zero fractional noise so `1,50`
  // and `1.5` collapse to the same `"1.5"`.
  const fracTrimmed = frac.replace(/0+$/, "");
  const canonical = fracTrimmed === "" ? whole : `${whole}.${fracTrimmed}`;
  // Reject zero (`0`, `0.0`, `00`) — strictly positive only.
  if (/^0+$/.test(canonical.replace(".", ""))) return null;
  return canonical;
}

/** S157 — format a line quantity (canonical dot-decimal string OR a
 * number, to tolerate both the new string wire shape and pre-S157
 * side-store rows that carry a JSON number) for read-only display in the
 * Hungarian convention: decimal **comma**, trailing zeros trimmed
 * (`1.5` → `1,5`, `1` → `1`, `0.25` → `0,25`). A non-numeric input passes
 * through verbatim so a backend drift is operator-visible rather than
 * silently zeroed (CLAUDE.md rule 12). */
export function formatQuantity(value: string | number): string {
  const n = typeof value === "number" ? value : Number(value);
  if (!Number.isFinite(n)) return String(value);
  // `Number` already drops trailing zeros; swap the `.` for the Hungarian
  // comma. (Quantities are small counts — no thousands grouping.)
  return String(n).replace(".", ",");
}
