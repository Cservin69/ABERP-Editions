// PR-91 — pure-module helpers for the Products master-data screen.
// Mirrors the partners.ts pattern: form state shape, empty defaults,
// wire→form / form→wire mappers, the typed validation-error parser.
// Pinned by `products.test.ts`.

import type {
  Currency,
  NavUnitOfMeasure,
  Product,
  ProductInputs,
  ProductUnit,
} from "./api";
import { formatMinorToInput, parseAmountToMinor } from "./format";
import { applySortDir, localeCompareHu, type SortDir } from "./list-sort";

/** PR-91 — every NAV unitOfMeasure token, paired with a Hungarian +
 * English operator-facing label. Order is roughly by Hungarian
 * commerce frequency: piece / time units first, then weight / volume
 * / distance / energy / packaging. The order is the dropdown's
 * display order.
 *
 * `OWN` is intentionally NOT here — it's the outer escape-hatch on
 * [`ProductUnit`]; the SPA's dropdown adds an "Egyéb (Own)" sentinel
 * which reveals a free-text input. See ADR-0046.
 *
 * Adding a token: extend the [`NavUnitOfMeasure`] union in `api.ts`,
 * add an entry here. The `nav_unit_serde_round_trip_pin` Rust test
 * + the SPA's exhaustive coverage pin in `products.test.ts` keep the
 * three surfaces (Rust enum / TS union / dropdown registry) in sync. */
export const NAV_UNIT_OPTIONS: ReadonlyArray<{
  token: NavUnitOfMeasure;
  label_hu: string;
  label_en: string;
}> = [
  { token: "PIECE", label_hu: "db (darab)", label_en: "Piece" },
  { token: "DAY", label_hu: "nap", label_en: "Day" },
  { token: "HOUR", label_hu: "óra", label_en: "Hour" },
  { token: "MINUTE", label_hu: "perc", label_en: "Minute" },
  { token: "MONTH", label_hu: "hónap", label_en: "Month" },
  { token: "KILOGRAM", label_hu: "kg", label_en: "Kilogram" },
  { token: "TON", label_hu: "tonna", label_en: "Ton" },
  { token: "LITER", label_hu: "liter", label_en: "Liter" },
  { token: "CUBIC_METER", label_hu: "m³", label_en: "Cubic meter" },
  { token: "METER", label_hu: "m", label_en: "Meter" },
  { token: "LINEAR_METER", label_hu: "fm (folyóméter)", label_en: "Linear meter" },
  { token: "KILOMETER", label_hu: "km", label_en: "Kilometer" },
  { token: "KWH", label_hu: "kWh", label_en: "Kilowatt-hour" },
  { token: "CARTON", label_hu: "karton", label_en: "Carton" },
  { token: "PACK", label_hu: "csomag", label_en: "Pack" },
];

/** PR-91 — sentinel selected when the operator wants a free-text
 * unit (the `Own` branch). The dropdown surfaces this as the LAST
 * option, labelled "Egyéb (Own)". The form reveals the free-text
 * input when this sentinel is selected; on submit the composer
 * translates it to `ProductUnit::Own(label)`. */
export const OWN_UNIT_SENTINEL = "__OWN__" as const;

/** PR-91 — operator-typed form state for the ProductForm modal.
 * Strings throughout so DOM `bind:value` round-trips cleanly. */
export interface ProductFormState {
  name: string;
  /** Either one of the [`NavUnitOfMeasure`] tokens or
   * [`OWN_UNIT_SENTINEL`]. */
  unitSelection: NavUnitOfMeasure | typeof OWN_UNIT_SENTINEL;
  /** Free-text label rendered only when `unitSelection === OWN_UNIT_SENTINEL`.
   * Ignored on submit otherwise. */
  unitOwnLabel: string;
  currency: Currency;
  /** Operator's typed unit-price string. Parsed via PR-88's
   * `parseAmountToMinor` on submit; same rules as the IssueInvoice
   * line editor (bare ints = WHOLE major units, `.` and `,` both
   * accepted as decimal separator, spaces/NBSP stripped). */
  unitPriceInput: string;
}

/** PR-91 — defaults for a freshly-opened ProductForm in create mode.
 * `PIECE` is the most-used unit (Ervin's `db` example); HUF is the
 * default currency (tenant base currency per ADR-0037). */
export function emptyProductForm(): ProductFormState {
  return {
    name: "",
    unitSelection: "PIECE",
    unitOwnLabel: "",
    currency: "HUF",
    unitPriceInput: "",
  };
}

/** PR-91 — fold a fetched Product into the form state for edit mode.
 * The price re-renders via [`formatMinorToInput`] (PR-88) so the
 * operator sees a canonical re-parseable string. */
export function formFromProduct(product: Product): ProductFormState {
  if (product.unit.kind === "Nav") {
    return {
      name: product.name,
      unitSelection: product.unit.value,
      unitOwnLabel: "",
      currency: product.currency,
      unitPriceInput: formatMinorToInput(product.unit_price_minor, product.currency),
    };
  }
  return {
    name: product.name,
    unitSelection: OWN_UNIT_SENTINEL,
    unitOwnLabel: product.unit.value,
    currency: product.currency,
    unitPriceInput: formatMinorToInput(product.unit_price_minor, product.currency),
  };
}

/** PR-91 — fold the form state into the wire `ProductInputs` body.
 * Pure; no DOM, no fetch. The price parser may return `null` for
 * malformed input — the composer maps that to `0` on the wire, the
 * backend's `validate_product_inputs` does not reject zero, so the
 * operator gets the catalog row saved with a zero price (a known
 * placeholder). If/when zero placeholders become undesirable the
 * validator gains a non-zero rule and the SPA renders the inline
 * error from the existing A157 envelope.
 *
 * The `Own` branch trims the label; the backend rejects empty-after-
 * trim via `validate_product_inputs`. */
export function composeProductInputs(form: ProductFormState): ProductInputs {
  const unit: ProductUnit =
    form.unitSelection === OWN_UNIT_SENTINEL
      ? { kind: "Own", value: form.unitOwnLabel.trim() }
      : { kind: "Nav", value: form.unitSelection };
  return {
    name: form.name.trim(),
    unit,
    currency: form.currency,
    unit_price_minor: parseAmountToMinor(form.unitPriceInput, form.currency) ?? 0,
  };
}

/** PR-91 — client-side admin-mode filter for the ProductsList screen.
 * Case-insensitive substring match on `name` (the only operator-
 * meaningful searchable field — units are dropdown-picked, price is
 * not a search target). Mirrors `filterPartners`. */
export function filterProducts(rows: Product[], needle: string): Product[] {
  const q = needle.trim().toLowerCase();
  if (q.length === 0) return rows;
  return rows.filter((p) => p.name.toLowerCase().includes(q));
}

/** PR-91 — operator-facing label for a product's unit. Hungarian by
 * default (the operator's locale); falls back to the raw NAV token
 * for unknown unions, falls through to the free-text label for
 * `Own`. Used by the list view's "Unit" column.
 *
 * `liter@15C` (the canonical Own case) renders verbatim — the label
 * IS the unit. */
export function unitLabel(unit: ProductUnit): string {
  if (unit.kind === "Own") return unit.value;
  const opt = NAV_UNIT_OPTIONS.find((o) => o.token === unit.value);
  return opt?.label_hu ?? unit.value;
}

// ─── PR-194 / session-194 — sortable columns + unit/currency facets ───
//
// S181 closed the needle-persistence gap; S194 lifts the ProductsList
// to parity with InvoiceList (S119 / S175): clickable column headers
// for Name / Unit / Currency / Unit price, two new facets (Unit auto-
// populated from rows + closed-vocab Currency HUF / EUR), and round-
// trip persistence for both new surfaces. Mirrors the partners.ts /
// invoice-list.ts posture: pure (a, b, key, dir) → number comparators
// so the renderer slices+sorts and the ES2019+ stable sort guarantee
// handles ties on top of the explicit `id` tiebreaker.

/** PR-194 — closed-vocab of sortable columns on ProductsList. */
export type ProductSortKey = "name" | "unit" | "currency" | "price";

/** PR-194 — Currency facet. `"All"` short-circuits the gate; the two
 * literal values mirror the closed-vocab `Currency` union (HUF + EUR
 * per ADR-0037 §3 — widening to a third lifts here). */
export type CurrencyFacet = "All" | Currency;

/** PR-194 — Unit facet. Stored as a stable `kind:value` key so the
 * NAV-token + Own-label namespaces don't collide (a Nav `LITER` and
 * an Own `liter` would render with the same label_hu but stay
 * distinct facets). `"All"` short-circuits. */
export type UnitFacet = "All" | string;

/** PR-194 — stable string key for a `ProductUnit`. Format:
 * `"Nav:<token>"` or `"Own:<free-text>"`. The renderer can split
 * this back into (kind, value) if it ever needs to; today it stays
 * opaque on the wire and the dropdown uses `unitLabel` to display
 * the human-readable form. */
export function unitFacetKey(unit: ProductUnit): string {
  return unit.kind === "Nav" ? `Nav:${unit.value}` : `Own:${unit.value}`;
}

/** PR-194 — quick-filter facet spec. AND-composed: a row must pass
 * every engaged facet to render. */
export interface ProductFilterSpec {
  needle: string;
  unit: UnitFacet;
  currency: CurrencyFacet;
}

/** PR-194 — empty filter (every facet open). */
export const EMPTY_PRODUCT_FILTER: ProductFilterSpec = {
  needle: "",
  unit: "All",
  currency: "All",
};

/** PR-194 — `true` iff every facet is open. */
export function isProductFilterEmpty(spec: ProductFilterSpec): boolean {
  return (
    spec.needle.trim().length === 0 &&
    spec.unit === "All" &&
    spec.currency === "All"
  );
}

/** PR-194 — facet + needle filter. Composes with `filterProducts`
 * (PR-91) so the existing `/`-search behaviour is unchanged when
 * only the needle is set. */
export function filterProductsWith(
  rows: Product[],
  spec: ProductFilterSpec,
): Product[] {
  const unitGated =
    spec.unit === "All"
      ? rows
      : rows.filter((p) => unitFacetKey(p.unit) === spec.unit);
  const currencyGated =
    spec.currency === "All"
      ? unitGated
      : unitGated.filter((p) => p.currency === spec.currency);
  return filterProducts(currencyGated, spec.needle);
}

function productIdTiebreak(a: Product, b: Product): number {
  if (a.id < b.id) return -1;
  if (a.id > b.id) return 1;
  return 0;
}

/** PR-194 — pure comparator. Locale-aware on string columns;
 * numeric on price (minor units of the row's currency — same
 * mixed-currency caveat as the invoice-list total comparator: a
 * €1 EUR product (100 cents) sorts between 99 HUF and 101 HUF, so
 * filter to a single currency before reading the sort meaningfully).
 * Ties go to `id` ascending. */
export function compareProducts(
  a: Product,
  b: Product,
  key: ProductSortKey,
  dir: SortDir,
): number {
  const raw = productRawCompare(a, b, key);
  if (raw !== 0) return applySortDir(raw, dir);
  return productIdTiebreak(a, b);
}

function productRawCompare(
  a: Product,
  b: Product,
  key: ProductSortKey,
): number {
  switch (key) {
    case "name":
      return localeCompareHu(a.name, b.name);
    case "unit":
      // Sort by the operator-visible label (Hungarian) so the
      // column's visual order matches the comparator's order. A
      // future addition to NAV_UNIT_OPTIONS that renames a label
      // shifts the sort position transparently.
      return localeCompareHu(unitLabel(a.unit), unitLabel(b.unit));
    case "currency":
      return localeCompareHu(a.currency, b.currency);
    case "price":
      return a.unit_price_minor - b.unit_price_minor;
  }
}

/** PR-194 — runtime list of legal sort keys. */
export const LEGAL_PRODUCT_SORT_KEYS: readonly ProductSortKey[] = [
  "name",
  "unit",
  "currency",
  "price",
];

/** PR-194 — runtime list of legal Currency facet values. */
export const LEGAL_CURRENCY_FACETS: readonly CurrencyFacet[] = [
  "All",
  "HUF",
  "EUR",
];

/** PR-91 — typed 400 validation body parser. Same shape as Partners
 * (the A157 inline-error envelope); the dispatcher accepts the
 * partners parser would too, but we duplicate the function here so a
 * future product-specific field-error type is a local widening. */
export function parseProductValidationError(
  raw: string,
):
  | { error: "validation_failed"; fields: Array<{ field: string; message: string }> }
  | null {
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start < 0 || end <= start) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.slice(start, end + 1));
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const obj = parsed as Record<string, unknown>;
  if (obj.error !== "validation_failed") return null;
  if (!Array.isArray(obj.fields)) return null;
  const fields: Array<{ field: string; message: string }> = [];
  for (const entry of obj.fields) {
    if (typeof entry !== "object" || entry === null) return null;
    const e = entry as Record<string, unknown>;
    if (typeof e.field !== "string" || typeof e.message !== "string") {
      return null;
    }
    fields.push({ field: e.field, message: e.message });
  }
  return { error: "validation_failed", fields };
}
