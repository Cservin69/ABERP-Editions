// PR-65 / session-86 — pure-module helpers for the SPA's
// list-row Tier-1 UX lift. Extends the composer-pin pattern
// (A156 / A161 / A163) so vitest pins the per-row buyer-label
// fallback + the quick-action filter table without mounting a
// Svelte 5 component (no `@testing-library/svelte` dep in this
// workspace; the project keeps the component-test runner
// name-deferred per CLAUDE.md rule 2 — pure helpers carry
// every load-bearing decision).
//
// Pinned by `invoice-list.test.ts`.

import type { DetailActionButton } from "./invoice-actions";
import { buttonsForState } from "./invoice-actions";
import type { Currency, InvoiceState, RowKind } from "./api";
import {
  PENDING_STATES,
  type OutgoingHygieneFacet,
} from "./hygiene-clickthrough";
import { lifecycleIndex } from "./labels";
import { filterInvoicesByNeedle, type InvoiceSearchRow } from "./keyboard-nav";

/** PR-65 / session-86 — quiet em-dash placeholder for the Partner
 * column when the backend has no buyer name to surface (CLI-issued
 * invoice, pre-PR-47α SPA-issued invoice, or a side-store I/O
 * failure). U+2014 (EM DASH) matches the ADR-0017 §1-2 quiet-
 * chrome posture for "no value to render here" cells. Pinned by
 * `buyer_column_display_em_dash_for_null` so a regression that
 * swaps the glyph for `"-"` / `"N/A"` / empty string surfaces at
 * `npm test`. */
export const PARTNER_COLUMN_EM_DASH = "—";

/** PR-65 / session-86 — fold a nullable buyer-name into the string
 * the list row's Partner cell renders. `null` / blank / whitespace-
 * only collapse to the em-dash placeholder so the operator never
 * sees a missing-buyer row as a totally empty cell (ambiguous with
 * "data not yet loaded"). The backend already trims + returns
 * `None` for blank values (`read_buyer_name_from_side_store`), so
 * the blank branch here is defence in depth.
 *
 * Returns a plain string the renderer drops into the cell verbatim
 * — the Svelte renderer applies the muted styling via a CSS class
 * on the `<td>`, NOT by inspecting the returned value, so the
 * em-dash glyph is the only signal both code paths share. */
export function buyerColumnDisplay(name: string | null): string {
  if (name === null) return PARTNER_COLUMN_EM_DASH;
  const trimmed = name.trim();
  return trimmed.length > 0 ? trimmed : PARTNER_COLUMN_EM_DASH;
}

/** PR-65 / session-86 — closed-vocab of per-row quick-action
 * buttons. Subset of the detail modal's [`DetailActionButton`]
 * vocab: only the three operator-named-as-load-bearing buttons
 * (Download / Submit / Storno) make sense as one-click row
 * affordances. PR-95 / session-115 — PollAck was removed from the
 * wider DetailActionButton vocab entirely; the NAV-status pictogram
 * in the state column now carries the poll affordance (clickable on
 * InFlight rows). Modification stays modal-only because it OPENS a
 * fresh form (the operator edits the corrected body), not a one-
 * click action. Per CLAUDE.md rule 3 (surgical) the narrow vocab is
 * the surface area the brief explicitly named. */
export type RowQuickAction = Extract<
  DetailActionButton,
  "Download" | "Submit" | "Storno" | "Pay" | "Delete"
>;

/** PR-65 / session-86 — filter the detail-modal button table down
 * to the row-quick-action buttons in operator-reading order.
 * Mirror invariant: the returned subset must preserve the per-state
 * gating of [`buttonsForState`] so a row-level click never bypasses
 * the precondition guard the backend enforces (a Storno button
 * appearing on a `Ready` row would produce a 409 the operator was
 * not warned about — exactly the failure mode A161 / A163 named).
 *
 * PR-70 / ADR-0039 — `paid` second parameter threads through to the
 * upstream `buttonsForState` so the row's Pay quick-action mirrors
 * the modal's Pay button gating. The output order places `Pay` after
 * `Submit` and before `Storno` so the operator-most-common flow
 * (Finalized → record payment) sits at a predictable column index.
 *
 * The output order mirrors the brief's `📄 PDF / ↗ Submit / 💰 Pay /
 * ⊘ Storno` left-to-right placement so the operator's eye finds each
 * glyph at a consistent column index across rows. Pinned by
 * `quick_actions_for_state_table_mirror`. */
export function quickActionsForState(
  state: InvoiceState,
  paid: boolean = false,
): RowQuickAction[] {
  const detail = buttonsForState(state, paid);
  const out: RowQuickAction[] = [];
  // Preserve a stable column order regardless of the detail table's
  // emission order. The detail table's per-state order is the
  // modal-header reading order; the list row's order is the column-
  // index order. Decoupling them keeps each surface's UX intent
  // independent.
  if (detail.includes("Download")) out.push("Download");
  // Session 162 — Submit surfaces as a row quick-action ONLY on `Ready`,
  // the one state where a click legally submits (`submit_invoice_request`
  // 409s a re-submit on any other state). `buttonsForState` also returns
  // Submit on the in-flight `Submitted` state, but there it is a DISABLED
  // "Beküldés folyamatban…" indicator the detail dialog renders — a row
  // quick-action has no disabled affordance, so a clickable row Submit on
  // `Submitted` would bypass the precondition guard and 409. Gating on
  // `Ready` keeps the row affordance clickable-legal (the mirror
  // invariant's intent) while the detail dialog owns the in-flight view.
  if (detail.includes("Submit") && state === "Ready") out.push("Submit");
  if (detail.includes("Pay")) out.push("Pay");
  if (detail.includes("Storno")) out.push("Storno");
  // S239 / PR-233 — surface the Delete row affordance on Draft rows.
  // Mirror of the `buttonsForState("Draft")` arm; the InvoiceList
  // gates the click on a confirmation modal that names any source
  // dispatch per [[hulye-biztos]].
  if (detail.includes("Delete")) out.push("Delete");
  return out;
}

/** PR-65 / session-86 — per-button presentational metadata for the
 * row-quick-action renderer. `glyph` is the leading icon (mirrors
 * the brief's `📄 / ↗ / ⊘`); `label` is the screen-reader + tooltip
 * text. Renderer composes both into the button (visual = glyph,
 * `aria-label` = `${label} invoice ${id}`). Pinned by
 * `quick_action_meta_table_round_trip` so a glyph drift surfaces. */
export interface QuickActionMeta {
  glyph: string;
  label: string;
}

export function quickActionMeta(action: RowQuickAction): QuickActionMeta {
  switch (action) {
    case "Download":
      return { glyph: "📄", label: "Download PDF" };
    case "Submit":
      return { glyph: "↗", label: "Submit to NAV" };
    case "Pay":
      return { glyph: "💰", label: "Mark as paid" };
    case "Storno":
      return { glyph: "⊘", label: "Cancel (storno)" };
    case "Delete":
      return { glyph: "🗑", label: "Delete draft" };
  }
}

// ─── PR-94 / session-114 — sortable columns + quick-filter facets ──────
//
// Two pure helpers extend the list-row Tier-1 UX surface as Ervin's
// invoice volume climbs ahead of the 2026-06-10 go-live:
//
//   1. `compareInvoices` — per-column comparator. Stable, direction-
//      aware, with "nulls last regardless of dir" for the optional
//      columns (partner, total). Tiebreaker on `invoice_id` so the
//      render order is reproducible across refreshes.
//   2. `filterInvoices` — facet filter (state + currency) composed
//      with the existing PR-68 needle filter from `./keyboard-nav`.
//      The facets AND with the needle (every filter must accept the
//      row).
//
// The comparator is a pure (a, b, key, dir) → number so the Svelte
// renderer can call `rows.slice().sort((a, b) => compareInvoices(a, b,
// sortKey, sortDir))` and rely on Array.prototype.sort's stable
// guarantee (ES2019+). Per the brief, all sorting is client-side over
// the loaded list — server-side pagination is named-deferred until
// the list outgrows a single fetch.

/** PR-94 / session-114 — closed-vocab of columns the operator can
 * sort by. Mirrors the renderable column set on `InvoiceList.svelte`
 * (invoice_id, invoice_number = fiscal_year + sequence_number,
 * partner = buyer_name, series_number = sequence_number, fiscal_year,
 * state, total = total_gross). No date column exists on the list-row
 * wire shape today (date columns are detail-modal-only); when one
 * lands as an addition to `InvoiceListItem`, lift it here as a new
 * key + a switch arm in [`compareInvoices`]. */
export type SortKey =
  | "invoice_id"
  | "invoice_number"
  | "partner"
  | "series_number"
  | "fiscal_year"
  | "state"
  | "total"
  | "row_kind";

/** PR-94 / session-114 — sort direction. */
export type SortDir = "asc" | "desc";

/** PR-94 / session-114 — structural shape the comparator inspects.
 * Mirrors the columns currently rendered on the list table; widening
 * `InvoiceListItem` later is a transparent additive change because
 * each `compareInvoices` consumer only reads the named columns. */
export interface InvoiceSortRow {
  invoice_id: string;
  sequence_number: number;
  fiscal_year: number;
  state: InvoiceState | string;
  total_gross: number | null;
  buyer_name: string | null;
  currency: Currency;
  /** PR-213 / S215 — virtual-union discriminator per ADR-0058. Read
   * by [`compareInvoices`] for the `row_kind` sort key (Own sorts
   * before ExtNav in ascending order — the operator's primary
   * concern is canonical rows; mirror rows are read-only context). */
  row_kind: RowKind;
}

/** PR-94 / session-114 — pure comparator. Returns a `(a, b) → number`
 * suitable for `Array.prototype.sort`. Stable in modern JS; ties go
 * to `invoice_id` ascending so the render order is reproducible
 * across refreshes (CLAUDE.md rule 12 — don't let render order silently
 * shuffle on identical inputs).
 *
 * Null discipline (CLAUDE.md rule 12 — fail visible, not silent):
 *
 *   - `partner` null → sorts AFTER every non-null partner regardless
 *     of `dir`. Operators reading the column want the meaningful
 *     values grouped; the em-dash placeholder cluster sits at the
 *     bottom whether ascending or descending. Same convention as
 *     spreadsheet apps (Excel / Numbers / Sheets) which all
 *     "nulls last" by default.
 *   - `total` null → same posture; no-total rows are drafts with no
 *     amount-meaningful sort position. Cluster at the bottom.
 *
 * Mixed-currency total sort caveat: `total_gross` is in MINOR units
 * of the row's currency (whole HUF for HUF, cents for EUR per
 * `InvoiceListItem.total_gross`'s contract). A mixed-currency list
 * sorted by total produces operator-surprising results (a €1 EUR
 * invoice (100 cents) sorts between 99 HUF and 101 HUF). The
 * currency-facet filter is the affordance for resolving this:
 * filter to a single currency, THEN sort by total. The comparator
 * itself stays pure and currency-blind so the helper is composable
 * with whatever facet the operator chose. Pinned by
 * `compare_invoices_total_uses_minor_units_not_string`. */
export function compareInvoices<R extends InvoiceSortRow>(
  a: R,
  b: R,
  key: SortKey,
  dir: SortDir,
): number {
  // Null-last carve-out FIRST — applied before the dir flip so the
  // sentinel side stays at the bottom regardless of ascending /
  // descending choice (operator's mental model: meaningful values
  // group at top in both directions; em-dash / no-total cluster sinks).
  const nullCmp = nullsLastCompare(a, b, key);
  if (nullCmp !== null) {
    // Both-null falls through to the invoice_id tiebreaker; one-side-
    // null returns the sentinel direction directly (no dir flip).
    if (nullCmp !== 0) return nullCmp;
    return invoiceIdTiebreak(a, b);
  }
  const cmp = rawCompare(a, b, key);
  if (cmp !== 0) return dir === "asc" ? cmp : -cmp;
  return invoiceIdTiebreak(a, b);
}

/** PR-94 / session-114 — invoice_id tiebreaker. Ascending regardless
 * of the user-selected sort dir. A regression that flipped the
 * tiebreaker with dir would silently re-shuffle the operator's display
 * on every dir toggle for tied rows (e.g., two rows with the same
 * total + currency); pinned by
 * `compareInvoices — ties + stability`. */
function invoiceIdTiebreak<R extends InvoiceSortRow>(a: R, b: R): number {
  if (a.invoice_id < b.invoice_id) return -1;
  if (a.invoice_id > b.invoice_id) return 1;
  return 0;
}

/** PR-94 / session-114 — null-handling carve-out. Returns:
 *   - `null` if neither side is null on the key (delegate to
 *     `rawCompare` for the typed compare).
 *   - `0` if both sides are null on the key (delegate to the
 *     invoice_id tiebreaker via the outer caller).
 *   - `1` if `a` is null + `b` is non-null (sort a AFTER b).
 *   - `-1` if `b` is null + `a` is non-null (sort b AFTER a).
 *
 * The return is dir-invariant — the outer caller does NOT apply the
 * dir flip. This is the load-bearing fix for the "nulls cluster at
 * the top when descending" regression that a naive flip would
 * produce. */
function nullsLastCompare<R extends InvoiceSortRow>(
  a: R,
  b: R,
  key: SortKey,
): number | null {
  switch (key) {
    case "partner": {
      const an = normalisePartner(a.buyer_name);
      const bn = normalisePartner(b.buyer_name);
      if (an === null && bn === null) return 0;
      if (an === null) return 1;
      if (bn === null) return -1;
      return null;
    }
    case "total": {
      if (a.total_gross === null && b.total_gross === null) return 0;
      if (a.total_gross === null) return 1;
      if (b.total_gross === null) return -1;
      return null;
    }
    default:
      return null;
  }
}

function rawCompare<R extends InvoiceSortRow>(
  a: R,
  b: R,
  key: SortKey,
): number {
  switch (key) {
    case "invoice_id":
      // ULIDs are lex-ordered = timestamp-ordered; string compare is
      // both correct and stable.
      if (a.invoice_id < b.invoice_id) return -1;
      if (a.invoice_id > b.invoice_id) return 1;
      return 0;
    case "invoice_number": {
      // Natural order on the composed `YYYY-NNNNNN`: fiscal_year
      // first, then sequence_number. Lexicographic compare on the
      // composed string would also work because both halves are
      // zero-padded fixed-width, but the tuple compare is the
      // load-bearing contract (a future change to either width would
      // silently break the string form).
      if (a.fiscal_year !== b.fiscal_year) return a.fiscal_year - b.fiscal_year;
      return a.sequence_number - b.sequence_number;
    }
    case "partner": {
      // Locale-aware compare on the trimmed name for operator-natural
      // ordering (Hungarian collation differs from byte-wise lex on
      // accented chars). Nulls handled upstream by `nullsLastCompare`.
      const an = normalisePartner(a.buyer_name);
      const bn = normalisePartner(b.buyer_name);
      // The non-null assertion is safe — `nullsLastCompare` already
      // returned non-null only when BOTH sides have a normalised name.
      return (an as string).localeCompare(bn as string);
    }
    case "series_number":
      return a.sequence_number - b.sequence_number;
    case "fiscal_year":
      return a.fiscal_year - b.fiscal_year;
    case "state":
      // Lifecycle-natural index per `labels.ts::LIFECYCLE_ORDER`.
      // Mirrors the default secondary sort the renderer already
      // applies; lifting it into the comparator lets the operator
      // click the State header to explicitly order by lifecycle.
      return lifecycleIndex(a.state) - lifecycleIndex(b.state);
    case "total": {
      // Numeric on minor units. Nulls handled upstream.
      return (a.total_gross as number) - (b.total_gross as number);
    }
    case "row_kind": {
      // PR-213 / S215 — Own < ExtNav in ascending order. The operator's
      // primary concern is the canonical AR set; the NAV-mirror rows are
      // read-only context that should cluster below by default. Within
      // the same kind the invoice_id tiebreaker takes over (assigned by
      // the outer `compareInvoices` wrapper).
      return rowKindIndex(a.row_kind) - rowKindIndex(b.row_kind);
    }
  }
}

/** S224 / PR-220 — can the operator open the InvoiceDetail modal for
 * a list row of this kind? `Own` rows live in the canonical `invoice`
 * table and have a `/api/invoices/:id` GET handler. `ExtNav` rows
 * live in `restored_invoice` (id prefix `rinv_*`) and have no
 * detail-fetch endpoint by design — they are NAV-mirror rows for
 * invoices ABERP did not issue, so there is no PDF, no audit trail,
 * and no lifecycle to inspect (per PR-213 / S215 architectural
 * invariant). PR-213 hid the actions-column affordance for ExtNav
 * but missed two other open-detail paths (chip click in
 * `InvoiceList.svelte` + `Enter` in the keyboard handler), so a
 * live operator click 404'd at `/invoices/rinv_…` (Ervin on
 * v2.1.4). The guard is now a single pure predicate both call
 * sites consult — adding a third `RowKind` variant widens this
 * switch alongside the type.
 *
 * Pinned by `canOpenDetail_*` cases. */
export function canOpenDetail(kind: RowKind): boolean {
  switch (kind) {
    case "Own":
      return true;
    case "ExtNav":
      return false;
  }
}

/** PR-213 / S215 — ordinal for the closed-vocab `RowKind`. Own < ExtNav
 * by design (see `compareInvoices`); adding a third variant to
 * `RowKind` widens both the type and this table together. */
function rowKindIndex(kind: RowKind): number {
  switch (kind) {
    case "Own":
      return 0;
    case "ExtNav":
      return 1;
  }
}

function normalisePartner(name: string | null): string | null {
  if (name === null) return null;
  const trimmed = name.trim();
  return trimmed.length > 0 ? trimmed : null;
}

/** PR-94 / session-114 — quick-filter facet spec. `state === "All"`
 * means "every state passes"; `currency === "All"` means "every
 * currency passes". `needle` is the same substring search the PR-68
 * `/` input drives — facets AND with the needle (every filter must
 * accept the row for the row to render). */
export interface InvoiceFilterSpec {
  needle: string;
  state: "All" | InvoiceState;
  currency: "All" | Currency;
  /** PR-213 / S215 — row-kind facet. `"All"` short-circuits the gate
   * (every row passes); `"Own"` restricts to canonical ABERP-issued
   * invoices; `"ExtNav"` restricts to NAV-mirror rows from
   * `restored_invoice`. */
  row_kind: "All" | RowKind;
  /** PR-223 / S227 — synthetic hygiene predicate driven by the
   * StatisticsPage click-through (no UI chip; URL-only init).
   * Optional (absent === open gate) so every pre-S227 call site
   * (`InvoiceList.svelte` chip-driven filter, persistence test
   * literals) continues to type-check without an explicit
   * `hygiene: null`. Closed-vocab when present:
   *   - `"pending"` — row.state ∈ [`PENDING_STATES`]; mirrors the
   *     dashboard's `outgoing_pending_count` (`reports::
   *     CountedKind::PendingDraft`).
   *   - `"no_partner"` — row.buyer_name is null / whitespace-only;
   *     combined with `row_kind = "ExtNav"` matches the dashboard's
   *     `restored_no_partner_count`. */
  hygiene?: OutgoingHygieneFacet | null;
}

/** PR-94 / session-114 — empty filter (every facet open). The
 * "Clear filters" button on the empty-state resets to this. Field
 * `hygiene` is intentionally OMITTED (not `hygiene: null`) so a
 * persistence round-trip on a pre-S227 blob deep-equals
 * `EMPTY_FILTER` — the validator never has to invent a field a
 * fresh install does not yet write. */
export const EMPTY_FILTER: InvoiceFilterSpec = {
  needle: "",
  state: "All",
  currency: "All",
  row_kind: "All",
};

/** PR-94 / session-114 — `true` iff the spec has every facet open
 * AND no search needle. Used by the renderer to decide whether to
 * surface the "Clear filters" button on the empty-state. */
export function isFilterEmpty(spec: InvoiceFilterSpec): boolean {
  return (
    spec.needle.trim().length === 0 &&
    spec.state === "All" &&
    spec.currency === "All" &&
    spec.row_kind === "All" &&
    // PR-223 / S227 — absent (undefined) and explicit-null both read
    // as "open gate". A typo in a URL param that the parser fell back
    // to null-on-unknown MUST still register as empty here so the
    // "Clear filters" button keeps its empty-state meaning.
    (spec.hygiene ?? null) === null
  );
}

/** PR-223 / S227 — pure predicate for the synthetic outgoing-hygiene
 * gate. Pure on the same `InvoiceSortRow` shape `filterInvoices`
 * already accepts; pinned by `hygiene-clickthrough.test.ts`. */
function passesOutgoingHygiene<R extends InvoiceSortRow>(
  row: R,
  hygiene: OutgoingHygieneFacet,
): boolean {
  switch (hygiene) {
    case "pending":
      return (PENDING_STATES as readonly string[]).includes(row.state);
    case "no_partner": {
      const name = row.buyer_name;
      if (name === null) return true;
      return name.trim().length === 0;
    }
  }
}

/** PR-94 / session-114 — facet + needle filter. Composes with
 * `filterInvoicesByNeedle` (PR-68) so the existing `/`-search
 * behaviour is unchanged when only the needle is set; the state +
 * currency facets AND with the needle on top.
 *
 * The state facet matches the row's `state` field exactly (case-
 * sensitive — the wire shape is the closed-vocab `InvoiceState`
 * union). The currency facet matches the row's `currency` field
 * exactly (closed-vocab `"HUF" | "EUR"`). An "All" value on either
 * facet short-circuits the predicate so the helper does no work
 * when the operator hasn't engaged the facet. */
export function filterInvoices<R extends InvoiceSortRow & InvoiceSearchRow>(
  rows: R[],
  spec: InvoiceFilterSpec,
): R[] {
  const stateGate = spec.state === "All";
  const currencyGate = spec.currency === "All";
  const rowKindGate = spec.row_kind === "All";
  const hygiene = spec.hygiene ?? null;
  const faceted = rows.filter((r) => {
    if (!stateGate && r.state !== spec.state) return false;
    if (!currencyGate && r.currency !== spec.currency) return false;
    if (!rowKindGate && r.row_kind !== spec.row_kind) return false;
    if (hygiene !== null && !passesOutgoingHygiene(r, hygiene)) return false;
    return true;
  });
  return filterInvoicesByNeedle(faceted, spec.needle);
}
