// PR-194 / session-194 — small pure-helper module shared by the
// PartnersList + ProductsList sort comparators (and re-usable by any
// future list that wants the same nulls-last + Hungarian locale
// posture). Mirrors the discipline the invoice-list comparator
// already encodes inline (PR-94 / session-114): nulls cluster at the
// bottom regardless of direction, locale-aware string compare for
// accented Hungarian characters (Á / É / Ö / Ő / Ű …), numeric
// compare for prices.
//
// Three exports:
//
//   - `localeCompareHu(a, b)` — Hungarian-collation string compare.
//     Wraps `Intl.Collator('hu')` via `String.prototype.localeCompare`
//     so accented characters sort in operator-natural order
//     (Á between A and B, not after Z).
//
//   - `compareNullishLast<T>(a, b, cmp)` — null-handling carve-out.
//     Returns:
//       * `null` if both sides are non-null (caller delegates to `cmp`)
//       * `0`    if both sides are null (caller delegates to its own
//                tiebreaker)
//       * `1`    if `a` is null, `b` is non-null  (sort a AFTER b)
//       * `-1`   if `b` is null, `a` is non-null  (sort b AFTER a)
//     The return is dir-invariant — the outer caller does NOT apply
//     the dir flip. This is the load-bearing fix for the "nulls
//     cluster at the top when descending" regression that a naive
//     flip would produce. Same posture as `invoice-list.ts ::
//     nullsLastCompare`; lifted here so the partners + products
//     comparators don't drift on the null discipline.
//
//   - `applySortDir(cmp, dir)` — flip the comparator's return for
//     descending. Trivial helper but keeps the call sites consistent.
//
// Pinned by `list-sort.test.ts`.

/** PR-194 — closed-vocab sort direction. Mirrors `invoice-list.ts ::
 * SortDir` so the persistence layer's validators recognise the same
 * literals. Kept duplicated rather than imported so this helper has
 * zero dependencies on the per-list modules (it's the bottom of the
 * stack). */
export type SortDir = "asc" | "desc";

/** PR-194 — Hungarian locale-aware string compare. Wraps
 * `String.prototype.localeCompare(other, 'hu')` so an operator
 * scanning the PartnersList Name column sees Á between A and B (the
 * native browser collator does this; a byte-wise lex sort would
 * cluster every accented character at the bottom of the ASCII
 * range). The `sensitivity: 'base'` option folds case + accent so
 * "Árpád" and "arpad" compare equal under a hypothetical case-blind
 * sort — but we keep the default (variant-sensitive) here so the
 * operator's typed casing survives the sort, matching how the
 * column already renders.
 *
 * Returns the standard tri-state (-1 / 0 / 1) compare result. */
export function localeCompareHu(a: string, b: string): number {
  return a.localeCompare(b, "hu");
}

/** PR-194 — null-discipline carve-out. See module docblock. The
 * caller passes both nullable values; this helper inspects only the
 * null-ness and returns the sentinel direction OR `null` to signal
 * "both non-null, please delegate to your typed compare". The `cmp`
 * argument is reserved for a future overload that closes the loop
 * inline; today's call sites prefer to branch explicitly so the
 * tiebreaker path stays visible. */
export function compareNullishLast<T>(
  a: T | null | undefined,
  b: T | null | undefined,
): number | null {
  const aNull = a === null || a === undefined;
  const bNull = b === null || b === undefined;
  if (aNull && bNull) return 0;
  if (aNull) return 1;
  if (bNull) return -1;
  return null;
}

/** PR-194 — flip the comparator's return value when the operator
 * requested descending order. Trivial helper kept as its own export
 * so the call site reads as `applySortDir(cmp, dir)` rather than
 * inlining the ternary at every comparator's tail. */
export function applySortDir(cmp: number, dir: SortDir): number {
  return dir === "asc" ? cmp : -cmp;
}
