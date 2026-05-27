// PR-84 — pure date helpers for the issue-invoice form's three-date UX.
//
// Three concepts, each with specific rules per the PR-84 brief:
//
//   1. Invoice date (Számla kelte) — always server-stamped today; never
//      typed by the operator. The form renders it read-only; no helper
//      here computes it (the server's clock is the truth).
//
//   2. Payment deadline (Fizetési határidő) — bidirectional: the
//      operator may type an offset in days (invoice date + N) OR an
//      absolute date, and the two fields update each other live. The
//      helpers `addDays` and `daysBetween` are the round-trip pair.
//
//   3. Delivery / fulfillment date (Teljesítési dátum) — REGULATORY
//      (NAV `invoiceDeliveryDate`; drives the VAT-period assignment).
//      The "comfort zone" is the closed interval [invoice_date,
//      payment_deadline]; out-of-range choices are allowed but pop a
//      soft "Are you sure?" confirmation and are audit-logged.
//
// Pure, no Svelte runes / no DOM — pinned under vitest. Calendar-date
// arithmetic only (YYYY-MM-DD strings); no timestamps, no time zones
// (Hungarian invoice dates are calendar dates, not instants).

/** Wire form for all three invoice dates — canonical ISO `YYYY-MM-DD`.
 * Matches the NAV `xs:date` shape (per `reference_nav_gotchas.md`
 * §"Date format") and the DuckDB `DATE` column form. */
export type IsoDate = string;

const ISO_DATE_RE = /^(\d{4})-(\d{2})-(\d{2})$/;

/** Default offset for the payment-deadline field on a fresh form (per
 * Ervin's HU-business convention: 8 days). The operator can override
 * to any non-negative integer; same-day (offset 0) is permitted and
 * means "due on the invoice date itself" (cash sale). */
export const DEFAULT_PAYMENT_OFFSET_DAYS = 8;

/** Parse an ISO date into a [year, month, day] triple. Returns null on
 * malformed input — the operator-typed input goes through this so the
 * form can surface a precise validation error rather than silently
 * substituting NaN downstream. */
export function parseIsoDate(s: IsoDate): { y: number; m: number; d: number } | null {
  const match = ISO_DATE_RE.exec(s);
  if (!match) return null;
  const y = Number(match[1]);
  const m = Number(match[2]);
  const d = Number(match[3]);
  if (m < 1 || m > 12 || d < 1 || d > 31) return null;
  // UTC noon avoids DST surprises across timezones; we only care about
  // the calendar date. Verify the parsed date round-trips (catches
  // overflow like "2026-02-30" which Date silently corrects).
  const dt = new Date(Date.UTC(y, m - 1, d, 12, 0, 0));
  if (dt.getUTCFullYear() !== y || dt.getUTCMonth() !== m - 1 || dt.getUTCDate() !== d) {
    return null;
  }
  return { y, m, d };
}

/** Format a Date (interpreted in UTC) into a YYYY-MM-DD string. */
function formatIsoFromUtc(dt: Date): IsoDate {
  const y = dt.getUTCFullYear();
  const m = String(dt.getUTCMonth() + 1).padStart(2, "0");
  const d = String(dt.getUTCDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

/** Today's date in the operator's local timezone, rendered as
 * YYYY-MM-DD. Used as the default for the read-only invoice-date
 * field on first paint. The server stamps the true issue date at
 * issuance time; this is a display default only. */
export function todayLocalIso(now: Date = new Date()): IsoDate {
  const y = now.getFullYear();
  const m = String(now.getMonth() + 1).padStart(2, "0");
  const d = String(now.getDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

/** Add N days to an ISO date and return the resulting ISO date. N may
 * be negative (subtract). Returns null if the input is malformed.
 * Handles month and year boundaries correctly (delegates to JS Date's
 * UTC arithmetic). */
export function addDays(iso: IsoDate, n: number): IsoDate | null {
  const parsed = parseIsoDate(iso);
  if (parsed === null) return null;
  if (!Number.isFinite(n) || !Number.isInteger(n)) return null;
  const dt = new Date(Date.UTC(parsed.y, parsed.m - 1, parsed.d, 12, 0, 0));
  dt.setUTCDate(dt.getUTCDate() + n);
  return formatIsoFromUtc(dt);
}

/** Calendar-day difference `toIso - fromIso`. Positive when `toIso` is
 * after `fromIso`, zero when equal, negative when before. Returns null
 * if either input is malformed. Round-trips with `addDays`:
 * `addDays(from, daysBetween(from, to)) === to`. */
export function daysBetween(fromIso: IsoDate, toIso: IsoDate): number | null {
  const a = parseIsoDate(fromIso);
  const b = parseIsoDate(toIso);
  if (a === null || b === null) return null;
  const da = Date.UTC(a.y, a.m - 1, a.d, 12, 0, 0);
  const db = Date.UTC(b.y, b.m - 1, b.d, 12, 0, 0);
  return Math.round((db - da) / 86_400_000);
}

/** Classification of a candidate delivery date against the
 * [invoice_date, payment_deadline] comfort zone. The form's
 * "Are you sure?" confirm fires for `BeforeInvoiceDate` and
 * `AfterPaymentDeadline`; `InRange` silently accepts the choice. The
 * audit payload stamps the same discriminant so the regulatory trail
 * records every out-of-range override.
 *
 * Bounds are INCLUSIVE — a delivery date equal to either endpoint is
 * in range (no confirm). */
export type ComfortZone = "InRange" | "BeforeInvoiceDate" | "AfterPaymentDeadline";

/** Classify a candidate delivery date against the comfort zone. The
 * comfort zone is the closed interval [invoice_date, payment_deadline].
 *
 * Returns null if any input is malformed OR if `payment_deadline` is
 * before `invoice_date` (a malformed range — the caller's form-level
 * validation should already have caught this, but we refuse to
 * classify against it rather than producing a wrong answer). */
export function comfortZone(
  invoiceDate: IsoDate,
  paymentDeadline: IsoDate,
  deliveryDate: IsoDate,
): ComfortZone | null {
  const beforeInvoice = daysBetween(invoiceDate, deliveryDate);
  const afterDeadline = daysBetween(paymentDeadline, deliveryDate);
  const rangeSpan = daysBetween(invoiceDate, paymentDeadline);
  if (beforeInvoice === null || afterDeadline === null || rangeSpan === null) {
    return null;
  }
  if (rangeSpan < 0) return null;
  if (beforeInvoice < 0) return "BeforeInvoiceDate";
  if (afterDeadline > 0) return "AfterPaymentDeadline";
  return "InRange";
}

/** Wire-form discriminant the audit payload records for the delivery-
 * date choice. `null` means "in range, no override" (default-path,
 * unflagged on the audit row). The two override values mirror the
 * `ComfortZone` enum's out-of-range arms verbatim. */
export type DeliveryDateOverride = "BeforeInvoiceDate" | "AfterPaymentDeadline" | null;

/** Map a `ComfortZone` classification to the audit-wire override
 * discriminant. `InRange` becomes `null`; the two out-of-range arms
 * map to themselves. Centralised so the SPA → backend wire form
 * stays consistent with the audit-payload field. */
export function overrideKindForZone(zone: ComfortZone): DeliveryDateOverride {
  if (zone === "InRange") return null;
  return zone;
}
