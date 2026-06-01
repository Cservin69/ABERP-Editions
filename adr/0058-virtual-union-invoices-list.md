# ADR-0058 — Virtual-union invoices list: ABERP-issued + NAV-mirror, UI-only `row_kind` discriminator, no schema change

**Status:** Accepted — S215 / PR-213 (2026-06-01). ABERP v2.1.
**Author:** Ervin Áben (ABERP), session 215 brief — first patch atop 2.0.
**Supersedes / amends:** none — additive architectural ADR. Names the
shape PR-213 shipped; pins the rule that future "where do
non-ABERP-issued invoices show up in the SPA list?" questions get
answered the same way.
**Related:** ADR-0019 (relational SoT — `invoice` is the regulated
surface, mirror tables stay separate), ADR-0036 (`InvoiceState`
lifecycle — the regulatory state vocab applies to `Own` rows only),
ADR-0017 (operator-facing UI density — closed-vocab + categorical
signals), the design doc `docs/external-outgoing-mirror-design.md`
(pre-implementation context; superseded by the addendum landed in this
PR), and the prior NAV-mirror infrastructure this PR re-reads
(S180 / PR-180 `restored_invoice` table, S196 catalog extraction).

## Context

ABERP's canonical AR set lives in the `invoice` DuckDB table — every
row there represents an invoice ABERP itself issued (allocator-burned
`(series_id, fiscal_year, sequence_number)` triple, audit-ledger
lifecycle, NAV submission). The tenant has historically issued
invoices through OTHER systems (Billingo, manual) earlier in the
year; NAV has those records too, queryable via
`queryInvoiceDigest OUTBOUND` keyed on the tenant's
`supplierTaxNumber`.

S180 / PR-180 added a separate-from-`invoice` mirror table
`restored_invoice` to support the NAV-as-DR wizard — that table
holds NAV-mirror rows safely away from the canonical surface (no
partner FK; no allocator state; UNIQUE defence per `(tenant_id,
source_nav_invoice_number)`). The pattern is battle-tested across
three Stage 1 features (`ap_invoice` for INBOUND, `restored_invoice`
for OUTBOUND-DR, S196 for OUTBOUND-catalog extraction).

The brief for PR-213: surface BOTH sets — canonical `invoice` rows
AND NAV-mirror `restored_invoice` rows — on the operator's invoices
list view, with an obvious "who issued this?" discriminator and the
hard guarantee that write-back affordances (Submit / Storno / Pay)
NEVER appear on the read-only NAV-mirror rows.

Two competing shapes were on the table per the design doc:

- **§5.1 — schema column.** Add `row_kind VARCHAR NOT NULL DEFAULT
  'restored'` to `restored_invoice`; backfill the legacy `'restored'`
  rows; add a CHECK on `{restored, external}`.
- **§5.2 — new table.** Stand up `external_outgoing_invoice` as a
  parallel mirror with its own daemon, audit kind, and SPA tab.

Ervin rejected both at the v2.1 brief — neither shape is necessary
and both buy operator confusion or migration risk that the actual
need does not justify.

## Decision

**Virtual union at the read layer; `row_kind` discriminator lives
ONLY on the wire shape and in the SPA render component. Neither
table gains a `row_kind` column.**

Operationally:

1. The `GET /api/invoices` handler reads BOTH tables. `invoice` rows
   pass through the existing audit-ledger-derived
   `derive_state` ladder with `row_kind: RowKind::Own`. The handler
   then queries `restored_invoice` via `restore_outgoing::list_restored`
   and synthesizes one `InvoiceListItem` per row with
   `row_kind: RowKind::ExtNav`, `state: InvoiceState::Unknown`,
   `sequence_number: 0`, `fiscal_year: restore_year`, every
   operational field (`payment`, `bank_account`, `buyer_name`) set
   to `None`, and `source_nav_invoice_number: Some(_)` carrying
   the raw NAV-emitted invoice number.

2. The wire shape extends `InvoiceListItem` with two fields:
   - `row_kind: RowKind` — closed-vocab `Own | ExtNav` enum,
     serialised as the PascalCase strings the SPA TS union
     consumes verbatim.
   - `source_nav_invoice_number: Option<String>` — `None` for `Own`
     rows (they carry their own `YYYY-NNNNNN` identifier);
     `Some(_)` for `ExtNav` rows (the operator-readable identity
     since the digest does not carry a buyer name).

3. The SPA renders a sortable `Kind` column (left-most), a row-kind
   facet filter, and a component-level guard
   `{#if row.row_kind === "Own"}` around the entire `<td
   class="col-actions">` body. ExtNav rows render the Kind chip + a
   muted `NAV: <invoice-number>` subtitle and nothing else
   actionable.

4. The Invoice id (UUID) column from PR-25 / PR-94 is dropped in
   the same PR — operators read invoices by `YYYY-NNNNNN`, not by
   ULID. The UUID is retained as the `#each` reactivity key for
   Svelte 5; the click-to-inspect affordance migrates to the Kind
   chip cell.

## Why this shape over a schema column

### 1. Application-layer invariants over DB-engine-specific schema

The codebase's convention is "closed-vocab columns that already
serve a business purpose get a `CHECK` constraint (currency,
local_status); we do NOT add CHECK / triggers to enforce
synthesised-view shape." Adding `row_kind` to `restored_invoice`
would have crossed that line — the discriminator is a property of
"which table did this row come from," not of "what does this row
mean." Computing it at read time keeps the invariant in code
(`fn restored_to_list_item` is the one site that mints
`RowKind::ExtNav`) and keeps the schema portable.

### 2. Surgical blast radius

A schema column would have required: a migration (`ALTER TABLE ADD
COLUMN`), a backfill (every legacy `restored_invoice` row gets
`'restored'`), an audit payload field on `InvoiceRestoredFromNavPayload`,
every read site updated to project the column, a CHECK constraint,
and a parallel SPA chip-filter retired against `local_status`-style
filtering. The UI-only union is two field additions on
`InvoiceListItem`, one mapping function (`fn restored_to_list_item`),
one new SPA column header, and one render-component guard. That's
the entire diff.

### 3. No phantom-write footgun

A schema column invites questions the read-time discriminator
sidesteps:
- "What happens when a `restored_invoice` row is later confirmed as
  ABERP-issued? Do we mutate `row_kind`?"
- "What happens when an `invoice` row is hand-deleted and the
  `restored_invoice` mirror still has it?"
- "What about legacy NULL `row_kind` rows from before the
  migration?"

The read-time union has none of these: each list response computes
the discriminator fresh from the row's source table; there is no
durable state to drift.

### 4. Audit-ledger unchanged

No new `EventKind` variant. The `system.*`-prefixed audit kinds
S180 / S196 / S178 / S210 established stay correct: the mirror
write paths still emit those; the list-view shape change does NOT
touch the ledger. This is load-bearing for the per-invoice export
bundle's `invoice.*` glob (it MUST never sweep mirror rows).

## Trade-offs

### What we give up

- **No persistent "is this row Own or ExtNav?" answer outside the
  list response.** A future audit-trail consumer that wanted to ask
  "which `restored_invoice` rows did the operator filter to in the
  list view yesterday?" would need to derive it from the list query
  every time. We accept this — no current consumer needs it.
- **No DB-level join across the two tables.** A future feature that
  wanted, say, "every invoice (Own or ExtNav) with `issue_date` in
  March 2026 — give me the union as a SQL view" would have to
  hand-write the UNION in application code. We accept this — the
  list-view consumer is the only consumer today.

### What we explicitly keep

- The S180 NAV-as-DR wizard's "show me already-restored rows"
  surface is unchanged. The `restored_invoice` table's existing
  list paths still work.
- The canonical `invoice` table is bit-identical to its PROD_v2.0
  shape. Operators who never enable the wizard see no behavioural
  change.
- Every other write path (issuance, NAV submit, mark-as-paid,
  storno, modification, email) is untouched and operates on `Own`
  rows only by construction — ExtNav rows have a `rinv_*` id
  prefix the route handlers' canonical-id lookups never resolve.

## Invariants pinned

1. **`RowKind` serde shape.** The strings `"Own"` and `"ExtNav"`
   are the wire form; a serde rename would break the SPA's
   `row.row_kind === "ExtNav"` action-hide guard. Pinned by
   `row_kind_serialises_as_pascal_case_strings` and by the TS
   union `type RowKind = "Own" | "ExtNav"` in `lib/api.ts`.

2. **ExtNav rows synthesise from `restored_invoice` only.** Every
   ExtNav row comes from `restored_to_list_item`; no other code
   path mints `RowKind::ExtNav`. Pinned by
   `restored_to_list_item_synthesises_ext_nav_row`.

3. **Unknown-currency arm loud-fails.** The DB's `CHECK (currency
   IN ('HUF','EUR'))` is defence in depth; the mapping function
   refuses to silently default. Pinned by
   `restored_to_list_item_loud_fails_unknown_currency`.

4. **SPA action-hide guard is a hard `{#if row.row_kind === "Own"}`,
   not a tooltip warning.** A regression that swapped this for
   `disabled` (operator can still see the affordance and hover-
   confuse) or a `class:disabled` would be caught at operator-survey
   time, not at compile time. The wire-shape pin
   (`invoice_list_item_emits_row_kind_and_source_nav_invoice_number`)
   guards the load-bearing field; the render-component shape is
   reviewed at code-review time.

5. **Sort order.** `Own < ExtNav` ascending; descending flips that.
   Within each cluster the `invoice_id` ascending tiebreaker takes
   over and does NOT flip with the sort direction. Pinned by
   `compareInvoices — row_kind column` in the SPA test suite.

## Open questions named-deferred

- **Promotion of an ExtNav row to canonical.** If the operator
  later issues the same invoice through ABERP, the canonical
  `invoice` row + the `restored_invoice` row both exist. Today
  the SPA shows both; a future enhancement could dedupe via
  `source_nav_invoice_number ↔ invoice's composed number` matching.
  Not part of v2.1.
- **Search by NAV invoice number.** `filterInvoicesByNeedle` does
  not yet include `source_nav_invoice_number` in its search
  haystack. Operators who type a Billingo invoice number into the
  search box do not get hits. The Kind facet is the affordance
  today; widen the needle search when an operator survey calls
  for it.
- **Server-side pagination over the unified set.** Today the
  union is computed client-side (every list response carries both
  sets). At the volumes Áben Consulting sees this is fine; a
  future tenant with >10k mirror rows would need server-side
  pagination, at which point the discriminator stays on the wire
  shape and pagination lives at the route handler.

## Alternatives considered (rejected)

- **§5.1 — schema column + backfill.** See §"Why this shape" for
  why this loses the application-layer-invariants and
  surgical-blast-radius arguments.
- **§5.2 — new `external_outgoing_invoice` table + new daemon +
  new SPA tab.** Same effort as a new module for a feature that's
  read-only and ~150 rows for the typical tenant. Operator
  confusion ("which view is canonical?") on top.
- **Tooltip-warning + `disabled` instead of hard hide.** Would
  surface the affordance, then 404 on click — operator-visible
  failure mode every time. The hard hide is the only safe shape.
- **No SPA changes; surface ExtNav rows only via a separate
  "Mirror" tab.** Conflates ABERP's own NAV-as-DR wizard surface
  with the operator's main list view. Two views on the same data,
  two mental models. Rejected as duplication.

## Implementation pointers

- `apps/aberp/src/serve.rs` — `enum RowKind`, `fn restored_to_list_item`,
  union loop at the end of `fn list_invoices`.
- `apps/aberp-ui/ui/src/lib/api.ts` — `type RowKind`,
  `InvoiceListItem.row_kind` + `source_nav_invoice_number`.
- `apps/aberp-ui/ui/src/lib/invoice-list.ts` — `SortKey` widened to
  include `"row_kind"`, `InvoiceFilterSpec.row_kind` facet,
  `compareInvoices` switch arm + `rowKindIndex` helper.
- `apps/aberp-ui/ui/src/lib/invoice-list-persistence.ts` —
  `LEGAL_ROW_KINDS` + `validateRowKindFacet` for the persisted
  view-prefs round trip.
- `apps/aberp-ui/ui/src/routes/InvoiceList.svelte` — Kind column
  header + cell, Kind facet dropdown, action-cell `{#if
  row.row_kind === "Own"}` guard, ext-nav-id subtitle and
  ext-nav-state-placeholder.
- `docs/external-outgoing-mirror-design.md` — Addendum recording
  the DECISION block; supersedes §5 and §6 of the body.
