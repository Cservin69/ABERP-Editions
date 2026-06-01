# Design — NAV outbound-mirror viability

**Document type:** Pre-implementation design review (no code).
**Date:** 2026-06-01.
**Author:** Claude (background design session).
**Status:** Review — Ervin to ratify or kill.

---

## 1. Verdict

**🟡 YELLOW — viable, but the proposed shape is the wrong shape.**

A literal reading of the brief — "new `external_outgoing_invoice` table + new auto-sync daemon + 4th SPA tab" — would work and would carry near-zero risk to canonical data. The pattern is already established twice (`ap_invoice` for INBOUND in S178, `restored_invoice` for OUTBOUND-DR in S180), so the implementation effort is small.

**However**, S180's `restored_invoice` table already mirrors `queryInvoiceDigest OUTBOUND` for a given year. The feature Ervin is asking for is **80 % delivered** by that surface today. What is missing is one small filter — "skip digests whose `invoice_number` is already in canonical `invoice`" — so the wizard's output is purely the **elsewhere-issued** subset.

Recommendation:

- **DO** extend S180's existing surface with the ABERP-issued skip filter + a lightweight ad-hoc trigger ("Show YTD elsewhere-issued") inside the existing `#/restore-from-nav` wizard. Effort: **S**.
- **DON'T** stand up a 4th mirror table + a 4th audit kind + a 4th SPA tab. That is `restored_invoice` warmed-over with extra ceremony and operator confusion ("Why are there two NAV-outbound views? Which one is canonical?"). Effort: **M**, value: marginal.
- **DEFER if Stage 2 is hot.** NAV's own portal (onlineszamla.nav.gov.hu) already shows this view. Operator already has Billingo's UI for elsewhere-issued visibility. This is Stage-1 polish; the friboard.com / e2e-shop strand (per [[e2e-shop-ground-zero-s199]] and [[aberp-2-0-cutover-s212]]) is the active frontier. Worth doing only after PR-E hardening.

Color summary:
- **GREEN** on technical viability (zero risk to canonical data, pattern proven).
- **YELLOW** on shape (the brief over-engineers it).
- **AMBER** on priority (Stage 2 has more leverage).

---

## 2. Context

Ervin's framing: "ABERP today only knows about invoices it ISSUED itself. The tenant has historically issued invoices through OTHER systems (Billingo, manual) earlier in the year. NAV has those records — every Hungarian-tax-registered invoice with `supplierTaxNumber=24904362-2-41` is in NAV's database whether ABERP issued it or not."

The ask: **a read-only mirror of current-year outgoing invoices for the tenant tax number that were NOT issued by ABERP itself**, so the operator gets a complete YTD outgoing view.

Hard constraint, repeated several times: "**not to mess already existing DB and data**." The feature must be entirely additive. This is **not** disaster-recovery (we have NAV-as-DR per S180 / S196); it's an operational-visibility surface for invoices issued outside ABERP in the tenant's name.

---

## 3. NAV capability

`queryInvoiceDigest OUTBOUND` is already wired and proven by S180 / PR-180 (`apps/aberp/src/restore_from_nav_outgoing.rs:555-585`). The SOAP call:

- Endpoint: `<base>/queryInvoiceDigest`.
- Request: `<QueryInvoiceDigestRequest>` with `<invoiceDirection>OUTBOUND</invoiceDirection>` and `<supplierTaxNumber>24904362</supplierTaxNumber>` (the 8-digit tax-number head — same shape ap_sync uses for INBOUND).
- Date range: NAV caps at 35 days per request; the existing daemon uses 30-day chunks for operator margin.
- Auth: per-request `<user>` block; no `exchangeToken`.
- Response: paginated `<invoiceDigest>` rows. Per the typed shape in `crates/nav-transport/src/operations/query_invoice_digest.rs:67-100`, the digest carries `invoice_number`, `supplier_tax_number`, `supplier_name`, `issue_date` (`YYYY-MM-DD`), `transaction_id`, `currency`, `invoice_net_amount`, `invoice_vat_amount`.
- Pagination: walks until `current_page >= available_page`. Existing daemons cap at 100 pages per cycle for safety.

What `queryInvoiceDigest` does **not** carry:
- `<customerInfo>` — no buyer name / address / tax number.
- `<invoiceLines>` — no line items.
- `<softwareId>` of the issuing system. **NAV does not tell you which software issued the invoice** from the digest. To recover that, you'd need to call `queryInvoiceData` per digest, base64-decode `<invoiceData>`, and read its `<softwareId>` — already implemented for S196's catalog-extraction path (`apps/aberp/src/restore_from_nav_extract.rs`).

For a YTD sweep of Áben Consulting's `24904362-2-41` tax number, NAV will return **every** outbound invoice ever submitted under that supplier head — ABERP-issued AND Billingo-issued AND any other tool's output. There is no NAV-side filter to ask only for "not-from-ABERP."

35-day chunking: the existing S180 wizard walks the year month-by-month (12 chunks); the existing S178 daemon walks a rolling 30-day window. Either pattern slots in without modification.

---

## 4. Detection of ABERP-issued vs elsewhere-issued

NAV doesn't tell us this directly in the digest. Three candidate strategies, ranked:

### 4.1 Chosen — presence in canonical `invoice` table

For each digest row, compute the canonical NAV invoice-number string ABERP would have emitted (`format!("{series_code}/{seq:05}", series_code, sequence_number)` per `apps/aberp/src/nav_xml.rs:544`) and check the existing `invoice` table for a row whose `(series_id, fiscal_year, sequence_number)` resolves to the same string. **If present → skip; if absent → mirror as external.**

Pros:
- Authoritative (the canonical record IS the SoT for "did ABERP issue this").
- Future-proof against an operator adding new series codes.
- Idempotent under arbitrary re-runs.

Cons:
- Requires a left-join-style read per digest. Trivial at digest volumes (low thousands per year).

### 4.2 Rejected — invoice-number pattern match

Compare digest `invoice_number` against the tenant's configured ABERP `[[numbering.series]]` prefixes from `seller.toml`. If matches → ABERP-issued; otherwise → external.

Rejected because:
- Operator-visible strings are mutable (series rename, fiscal-year rollover); a pattern match would mis-classify after any config drift.
- Billingo's numbers might collide with ABERP's prefix string-shape by chance.
- Strategy 4.1 is strictly more correct.

### 4.3 Rejected — `softwareId` extraction via `queryInvoiceData`

For each digest, fetch `queryInvoiceData OUTBOUND`, base64-decode `<invoiceData>`, read `<softwareId>`, compare against ABERP's compile-time `softwareId` (`apps/aberp/src/build_profile.rs`).

Rejected because:
- One extra NAV round-trip per digest (the S180 wizard already pays this cost in S196 for partner+product extraction, but for a pure-visibility feature it's overkill).
- Strategy 4.1 is faster and at least as correct.
- `softwareId` could in principle be spoofed at submission time; strategy 4.1 is grounded in the local record.

---

## 5. Storage

### 5.1 Recommended: reuse `restored_invoice` + add a row-type discriminator

S180's `restored_invoice` (`apps/aberp/src/restore_from_nav_outgoing.rs:167-186`) already mirrors `queryInvoiceDigest OUTBOUND` digests into a separate-from-`invoice` table with `UNIQUE (tenant_id, source_nav_invoice_number)` defence and `CHECK (currency IN ('HUF','EUR'))`. The column shape is identical to what an "elsewhere-issued" mirror needs:

```sql
-- ACTUAL existing schema, NOT a new design:
CREATE TABLE IF NOT EXISTS restored_invoice (
    id                          VARCHAR NOT NULL PRIMARY KEY,
    tenant_id                   VARCHAR NOT NULL,
    source_nav_invoice_number   VARCHAR NOT NULL,
    source_nav_transaction_id   VARCHAR,
    issue_date                  VARCHAR NOT NULL,
    total_net_minor             BIGINT  NOT NULL,
    total_vat_minor             BIGINT  NOT NULL,
    total_gross_minor           BIGINT  NOT NULL,
    currency                    VARCHAR NOT NULL CHECK (currency IN ('HUF','EUR')),
    restore_year                INTEGER NOT NULL,
    created_at                  VARCHAR NOT NULL,
    UNIQUE (tenant_id, source_nav_invoice_number)
);
```

Two minimum-blast-radius additions:
1. **`row_kind VARCHAR NOT NULL DEFAULT 'restored'` column.** Closed-vocab `'restored' | 'external'`. Pre-existing rows backfill to `'restored'`.
2. **Filter switch on the wizard trigger.** A boolean operator-input "Skip ABERP-issued" runs the strategy-4.1 filter inline during the digest walk.

The SPA list view already has `local_status`-style chip filtering (`apps/aberp-ui/ui/src/routes/RestoreFromNavWizard.svelte:245`); a `row_kind` chip filter falls out for free.

### 5.2 If a new table is unavoidable

If Ervin insists on a separate table for mental-model cleanliness, the shape is near-verbatim `restored_invoice`:

```sql
-- HYPOTHETICAL — only if 5.1 is rejected:
CREATE TABLE IF NOT EXISTS external_outgoing_invoice (
    id                        VARCHAR NOT NULL PRIMARY KEY,
    tenant_id                 VARCHAR NOT NULL,
    nav_invoice_number        VARCHAR NOT NULL,
    nav_transaction_id        VARCHAR,
    issue_date                VARCHAR NOT NULL,
    total_net_minor           BIGINT  NOT NULL,
    total_vat_minor           BIGINT  NOT NULL,
    total_gross_minor         BIGINT  NOT NULL,
    currency                  VARCHAR NOT NULL CHECK (currency IN ('HUF','EUR')),
    mirrored_at               VARCHAR NOT NULL,
    nav_xml_path              VARCHAR,   -- nullable; set by S197-style fetch
    UNIQUE (tenant_id, nav_invoice_number)
);
```

ID prefix `eoinv_<ULID>` to keep [[id-prefixes]] tidy.

### 5.3 Aside — the brief's [[no-sql-specific]] claim is wrong

The brief cites a memory `[[no-sql-specific]]` to argue "invariants in app layer, never DB CHECK/triggers." **No such memory file exists** in `/Users/aben/.claude/projects/-Users-aben-Documents-Claude-Projects-ABERP/memory/`, and the codebase visibly disagrees:

- `ap_invoice` uses `CHECK (currency IN ('HUF','EUR'))` and `CHECK (local_status IN ('Outstanding','Paid','Irrelevant'))` (`apps/aberp/src/incoming_invoices.rs:391-392`).
- `restored_invoice` uses `CHECK (currency IN ('HUF','EUR'))` (`restore_from_nav_outgoing.rs:177`).
- `invoice_line` uses `CHECK (quantity >= 0)` (`modules/billing/src/adapters/duckdb_store.rs:173`).

This design follows the codebase's actual convention: closed-vocab columns get a DB-level `CHECK`. Defence in depth: the app-layer enum + the DB-layer constraint catch each other's bugs.

---

## 6. SPA shape

Options:

### 6.1 Recommended — fold into existing `#/restore-from-nav` wizard

Add one operator toggle inside the existing wizard: **"Only show invoices NOT issued by ABERP"**. Default off (preserves S180 DR behaviour); when on, the year-walk applies strategy 4.1 and only writes elsewhere-issued rows.

Result: the same `restored_invoice` list view shows the operator a year's worth of elsewhere-issued invoices, distinguishable by the `row_kind` chip.

Cost: one checkbox, one boolean threaded through `walk_month`, one chip in the list.

### 6.2 Alternative — new 4th tab on Invoices page

The existing tab persistence (`apps/aberp-ui/ui/src/lib/invoice-tab-persistence.ts`) already takes `"outgoing" | "incoming" | "quotes"`. Adding `"external"` is a one-line widening + a new `ExternalOutgoingList.svelte` route component.

Cost: meaningfully higher than 6.1 (new tab, new route component, new card-style + chip-row), and conflates with `restored_invoice`'s existing surface ("which view is canonical?"). Don't.

### 6.3 Rejected — filter chip inside existing Outgoing tab

Showing elsewhere-issued rows alongside canonical `invoice` rows in the Outgoing tab conflates two different data sources (canonical AR vs read-only NAV mirror) and breaks every action button on the row (edit, storno, email — none of which apply to elsewhere-issued rows). Don't.

---

## 7. Risk to existing data (the operator's main worry)

Enumerated zero-risk paths:

| Risk | Mitigation | Confidence |
|---|---|---|
| Bootstrap accidentally inserts into canonical `invoice` | Writes only to `restored_invoice` (or `external_outgoing_invoice`). The canonical `invoice` table requires `customer_id NOT NULL` → partners FK, which the mirror never satisfies. | **High** — caught at the SQL layer. |
| Canonical `(series_id, fiscal_year, sequence_number)` allocator gets perturbed | Mirror writes never touch `invoice_sequence_state`. The gap-free allocator is wholly inside `modules/billing/src/adapters/duckdb_store.rs`. | **High** — physically separate tables. |
| Audit ledger gains canonical `InvoiceDraftCreated` / `InvoiceIssued` rows for elsewhere-issued invoices | Mirror emits ONLY `system.*`-prefixed kinds (see §8). The `invoice.*` glob in the per-invoice export bundle never sweeps `system.*` rows — this invariant is already pinned by S180 / S196 / S178 docstrings. | **High** — pattern is battle-tested 3×. |
| NAV mirror triggers a NAV submission | Read-only. `queryInvoiceDigest` and `queryInvoiceData` are non-mutating SOAP ops. | **Certain**. |
| Operator issues an invoice N1 in ABERP at 14:00; bootstrap polls at 14:01; N1 appears in BOTH `invoice` AND `restored_invoice` | Strategy 4.1 skip filter catches this on the very same cycle (the canonical row exists at the time of mirror write). | **High** — single-process serialization (`INGEST_SERIALIZER` already pins this for AP). |
| Operator first issues N1 in Billingo, then later runs the wizard, then later starts issuing N1's number range in ABERP | Impossible — the gap-free allocator picks the next series-`sequence_number`; a number once used in Billingo cannot be re-used by ABERP for the same `(series_id, fiscal_year)`. If the operator deliberately collides, that's an operator error the canonical UNIQUE catches. | **High**. |
| `restored_invoice` row with `row_kind='external'` for an invoice that LATER gets ABERP-issued (impossible if §4.1 ran first, but conceivable if the operator wipes canonical state) | On every cycle, re-check `row_kind='external'` rows against canonical `invoice`; DELETE the stale mirror row. **Or simpler: don't bother.** A stale mirror row is harmless visual duplication, manually removable via SPA. | **High** — pure UX. |

**Net assessment:** The risk to existing data is effectively zero by construction, **provided** the mirror table is separate from canonical `invoice` (which both §5.1 and §5.2 ensure). Ervin's concern is correctly addressed by the existing pattern.

---

## 8. Audit ledger

### 8.1 If §5.1 (reuse `restored_invoice`)

No new event kind. Reuse `EventKind::InvoiceRestoredFromNav` (`crates/audit-ledger/src/entry/event_kind.rs:716`) and extend its payload with `row_kind: "restored" | "external"`. The wizard already writes ONE entry per inserted row.

### 8.2 If §5.2 (new table)

Per the F12 four-edit ritual, one new variant:

```
EventKind::ExternalOutgoingMirrorPollCompleted
  → on-disk str: "system.external_outgoing_mirror_poll_completed"
```

Payload (`ExternalOutgoingMirrorPollCompletedPayload`): `trigger` (`"daemon" | "manual"`), `date_from`, `date_to`, `mirrored_count`, `skipped_count` (ABERP-issued + already-mirrored), `pages_walked`, `elapsed_ms`, `error: Option<String>`.

The `system.` prefix is load-bearing: the per-OUTGOING-invoice export bundle's `invoice.*` glob MUST NEVER sweep mirror rows. Same posture as `IncomingInvoiceSyncCycleCompleted` and `InvoiceRestoredFromNav`.

---

## 9. Implementation effort

If §5.1 (reuse `restored_invoice`):

| Area | Effort | Notes |
|---|---|---|
| Backend — add `row_kind` column + migration | S | One-line ADD COLUMN IF NOT EXISTS + backfill `'restored'`. |
| Backend — thread `skip_aberp_issued: bool` through `walk_month` | S | One parameter, one filter inside the digest loop. |
| Backend — `is_aberp_issued(invoice_number)` helper | S | Compose canonical number from `invoice` rows; HashSet lookup at month-walk start (S186 pattern). |
| Backend — extend `InvoiceRestoredFromNavPayload` with `row_kind` | S | Additive serde field (`#[serde(default)]`). |
| SPA — wizard checkbox + chip | S | Existing `RestoreFromNavWizard.svelte`. |
| SPA — list chip filter on `row_kind` | S | Existing filter row. |
| Tests — extend S180 + S196 test fixtures with mixed-kind cycles | S | 6-8 new pins. |
| **Total** | **~1 day** | If on-the-shoulders of S180. |

If §5.2 (new `external_outgoing_invoice` table) — add ~1 more day for the new table + new daemon + new route + new SPA tab + new audit kind.

---

## 10. Where I disagree with the premise

Per [[pushback-as-method]]:

### 10.1 Wrong shape — the brief's "new table + new daemon + new tab" duplicates `restored_invoice`

`restored_invoice` already mirrors `queryInvoiceDigest OUTBOUND` into a separate-from-`invoice` table with UNIQUE defence. The brief's "external_outgoing_invoice" is `restored_invoice` warmed-over with a different name. The cost of two parallel surfaces is operator confusion ("which one is canonical?"), code duplication, and an extra audit kind. Adding `row_kind` to `restored_invoice` is the smaller change with the same outcome.

### 10.2 Wrong claim — the [[no-sql-specific]] memory citation is fabricated

The brief asserts `[[no-sql-specific]]` exists and reads "invariants in app layer, never DB CHECK/triggers." That memory file does NOT exist in this project's memory directory, and the codebase uses CHECK constraints throughout (`ap_invoice.currency`, `ap_invoice.local_status`, `restored_invoice.currency`, `invoice_line.quantity >= 0`). Same for `[[trust-code-not-operator]]` and `[[aberp-nav-as-dr]]` — none of these memory slugs exist. Likely the brief was assembled by a previous agent without reading the actual memory index; this design follows the **actual** codebase convention (CHECK at the DB + matching enum/closed-vocab in Rust).

### 10.3 Wrong priority — Stage 2 has more leverage

Per [[aberp-2-0-cutover-s212]] and [[e2e-shop-ground-zero-s199]], the friboard.com / e2e-shop strand is the active frontier. NAV's own portal (onlineszamla.nav.gov.hu) already shows the elsewhere-issued view; Billingo's UI shows the Billingo-issued view; the operator has working visibility today, just not consolidated inside ABERP. Stage-1 polish in ABERP is fine but should wait until PR-E hardening lands. Estimated value: **operator UX comfort** (mid). Estimated cost: **~1 day** (low). ROI: positive but **not urgent**.

### 10.4 Possibly wrong scope — "current year only" is operator-facing, not legally meaningful

The brief specifies "current-year" as the window. ÁFA reporting is monthly; ten-year retention is the legal floor. If the operator's real concern is YTD visibility, then current-year is fine. If the real concern is "I'd like to see all my invoices in one place," then the wizard should let the operator pick any year (which S180 already does). Worth a 30-second confirmation with Ervin before coding.

### 10.5 Possibly wrong assumption — "non-ABERP invoices exist in 2026"

Áben Consulting's PROD_v2.0 cutover landed at PR-211 (1db48bb). If the operator switched to ABERP for AR at the cutover and stopped using Billingo, then the elsewhere-issued set might be the (frozen) pre-cutover prefix of 2026 only. In that case a **one-shot import** is simpler than a daemon — just run the wizard once, mirror that frozen prefix, and never poll again. The daemon overhead is wasted on a frozen set.

---

## 11. Open questions for Ervin

1. **Reuse `restored_invoice` or new table?** Recommendation: reuse + `row_kind` discriminator (§5.1). One-line override if you disagree.
2. **One-shot or daemon?** If you stopped issuing via Billingo at PROD_v2.0 cutover, the elsewhere-issued 2026 set is frozen — one-shot import via the wizard suffices. If you're still issuing via Billingo for some workflow, a 30-min poll daemon is justified.
3. **Wizard checkbox vs invisible default?** I'd default the new "Skip ABERP-issued" checkbox to **on** when the operator's main goal is YTD visibility (S180's original DR-restore path would explicitly uncheck it).
4. **Priority vs Stage 2?** Estimated 1 day of work. Worth ~1 day off the e2e-shop strand?
5. **Year selector?** S180 already takes an arbitrary year in [2018, current]. The brief says "current-year"; suggest reusing S180's full picker rather than special-casing.

---

## Appendix — references

- `crates/nav-transport/src/operations/query_invoice_digest.rs` — typed `InvoiceDigest`, `parse_digest_page`, OUTBOUND/INBOUND wiring.
- `crates/nav-transport/src/operations/query_invoice_data.rs` — full-XML fetch (used by S196 OUTBOUND, S197 INBOUND).
- `crates/nav-transport/src/soap/mod.rs:413-454` — `InvoiceDirection` enum.
- `apps/aberp/src/restore_from_nav_outgoing.rs` — S180 DR-restore: month-walk, `restored_invoice` table, idempotency via audit-ledger scan.
- `apps/aberp/src/restore_from_nav_extract.rs` — S196 catalog extraction from `queryInvoiceData OUTBOUND`.
- `apps/aberp/src/ap_sync.rs` — S178 / S197 INBOUND mirror daemon (30-min cadence, S203 bootstrap-year sweep).
- `apps/aberp/src/incoming_invoices.rs:377-403` — `ap_invoice` schema with `CHECK (currency IN ('HUF','EUR'))`.
- `modules/billing/src/adapters/duckdb_store.rs:78-161` — canonical `invoice` schema with `customer_id NOT NULL`, `(series_id, fiscal_year, sequence_number)` UNIQUE.
- `crates/audit-ledger/src/entry/event_kind.rs:674-746` — `IncomingInvoiceSyncCycleCompleted`, `InvoiceRestoredFromNav`, `QuoteIntakePollCompleted` (all `system.`-prefixed).
- `apps/aberp-ui/ui/src/lib/invoice-tab-persistence.ts` — 3-tab closed-vocab (`outgoing | incoming | quotes`).
- `apps/aberp-ui/ui/src/App.svelte:575-581` — tab-router structure.

---

## Addendum — DECISION (Ervin, 2026-06-01)

**Resolved: UI-only union, no schema change.** Ervin ratified neither §5.1
(reuse `restored_invoice` + `row_kind` column) nor §5.2 (new
`external_outgoing_invoice` table). The implementation that landed in
**PR-213 / ABERP v2.1** is:

- **DB untouched.** Both `invoice` (canonical) and `restored_invoice`
  (S180 NAV-as-DR mirror) keep their existing schema verbatim — no
  `row_kind` column anywhere, no migration, no new table.
- **Read-time virtual union in `list_invoices`.** The handler reads
  both tables and synthesizes a flat `Vec<InvoiceListItem>` where each
  row carries a `row_kind: RowKind` (`Own | ExtNav`) discriminator on
  the wire shape only.
- **SPA renders the Kind chip + hard-hides every write-back action on
  ExtNav rows.** Sort / filter on `row_kind` are first-class (same
  shape as the existing State + Currency facets).
- **Same PR drops the UUID column from the list view** (operators read
  invoices by `2026-000054`, not by ULID) and surfaces the source NAV
  invoice number as a muted subtitle on ExtNav rows.

### Why the read-time union over a schema column

1. **DB-engine portability.** Invariants live in the application layer
   per the project's "no SQL-specific schema-time enforcement of
   business rules" lean — DuckDB-specific `CHECK` constraints are kept
   on closed-vocab columns that already exist (`currency`,
   `local_status`); we do NOT add new ones for synthesised view shape.
2. **Surgical blast radius.** Adding a `row_kind` column to
   `restored_invoice` would have meant a migration, backfill, every
   read/write site updated, and a new audit-payload field. The
   UI-layer union is a `Vec::push` loop in `list_invoices` + one new
   SPA column.
3. **No phantom canonical write path.** A schema column would have
   invited "what about a NULL row_kind on legacy rows" / "what about
   restored_invoice rows that get promoted to canonical" footguns. The
   read-time discriminator has neither — it's computed fresh from the
   row's table of origin on every list response.
4. **Audit ledger unchanged.** No new `EventKind` variant. The
   `restored_invoice` mirror is already pinned to `system.*` audit
   kinds; the list-view shape change does not touch the ledger.

### What this addendum supersedes in the body of this document

§5 ("Storage") and §6 ("SPA shape") are **historical context only**.
Both 5.1 (reuse + `row_kind` column) and 5.2 (new
`external_outgoing_invoice` table) are rejected in favour of the
UI-only read-time union above. The risk-assessment table in §7 still
applies verbatim (mirror writes are still non-canonical; the
allocator is still untouched).

### Where the invariant lives

- Backend: `apps/aberp/src/serve.rs` — `enum RowKind { Own, ExtNav }`,
  `fn restored_to_list_item`, and the union loop at the end of
  `fn list_invoices`.
- SPA: `apps/aberp-ui/ui/src/routes/InvoiceList.svelte` — `Kind`
  column + the `{#if row.row_kind === "Own"}` action-cell guard.
- ADR: `adr/0058-virtual-union-invoices-list.md` records the
  architectural decision in the standard ADR shape so a future
  reviewer finds the rationale without digging through this
  pre-implementation review.

---

**End of document.**
