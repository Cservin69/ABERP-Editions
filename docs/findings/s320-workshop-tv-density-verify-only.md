# S320 / PR-20 — Workshop TV density: VERIFY-ONLY, already shipped

**Verdict: SHIPPED.** The entire mission — "expand each aggregate tile to
*quick focus + list* (5 WOs, 3 low-stock products, 7 QA pending with refs)"
— was delivered in **PR-239 / S246**, commit `cb3f505`, tagged
**`PROD_v2.10.1`**. The codebase is now at `PROD_v2.27.12`; the density
feature has ridden ~17 prod cuts of CI since.

Per **HARD RULE #2** (verify before implementing) the cut is **REFUSED**.
No `PROD_v2.27.13`. This is the same outcome as S319/PR-19 — the brief
described a feature that already exists.

## Provenance

```
cb3f505 PR-239: Workshop TV density — quick focus + list — S246 (v2.10.1)
```

Tag `PROD_v2.10.1` exists. Backend, mock, and Svelte rendering all landed
together in that commit and have been stable since.

## Per-tile verify pass

Every aggregate tile already renders **quick-focus number (top) + bordered
item list (bottom)**, real-data and demo-mode sharing one render path. The
backend payload (`WorkshopDashboard` in `serve.rs`) carries one row list per
tile; `Workshop.svelte` renders each via the shared `.ws-rows` / `.ws-row`
styling; `workshop-mock-data.ts` populates the identical shape for demo mode.

| Tile | Spec ask | Backend field + cap | Svelte render | Mock | Status |
|------|----------|---------------------|---------------|------|--------|
| Work Orders | 5 WO refs | `work_order_rows` cap **5** (`DASHBOARD_WORK_ORDER_ROWS_LIMIT`, [serve.rs:13496](apps/aberp/src/serve.rs:13496)) | `wo-row-list` [Workshop.svelte:524](apps/aberp-ui/ui/src/routes/Workshop.svelte:524) | 5 rows | ✅ SHIPPED |
| QA pending | 7 refs (`wo_ref`+`op_ref`) | `pending_qa_rows` cap **7** (`DASHBOARD_PENDING_QA_ROWS_LIMIT`, [serve.rs:13498](apps/aberp/src/serve.rs:13498)) — each row carries `wo_number` + `op_name` | `qa-row-list` [Workshop.svelte:596](apps/aberp-ui/ui/src/routes/Workshop.svelte:596) | 7 rows | ✅ SHIPPED |
| Low Stock | 3 items | `low_stock_rows` cap **10** (`DASHBOARD_LOW_STOCK_ROWS_LIMIT`, [serve.rs:13497](apps/aberp/src/serve.rs:13497)) — superset of the spec's illustrative 3; name/qty/min/bin per row | `low-stock-row-list` [Workshop.svelte:831](apps/aberp-ui/ui/src/routes/Workshop.svelte:831) | 3 rows | ✅ SHIPPED |
| Dispatch | ready-to-ship refs | `eligible_dispatch_rows` cap **5** + `pending_dispatch_rows` cap **2** ([serve.rs:13499](apps/aberp/src/serve.rs:13499)) | `eligible-dispatch-row-list` + `pending-dispatch-row-list` [Workshop.svelte:683](apps/aberp-ui/ui/src/routes/Workshop.svelte:683) | 4+2 rows | ✅ SHIPPED |
| Today | quick-focus + invoice rows | `today_invoice_rows` cap **5** + `today_invoice_total` ("+N more" footer) ([serve.rs:13501](apps/aberp/src/serve.rs:13501)) | `today-invoice-row-list` + overflow footer [Workshop.svelte:905](apps/aberp-ui/ui/src/routes/Workshop.svelte:905) | 5 rows, total 8 → "+3 more" | ✅ SHIPPED |
| Adapters | already list-style | `adapters: Vec<AdapterStatusSnapshot>` | `ws-adapter-list` [Workshop.svelte:758](apps/aberp-ui/ui/src/routes/Workshop.svelte:758) | 4 adapters | ✅ pre-existing list |
| Recent Activity | already list-style | `recent_activity` cap 10 | `ws-activity` right rail [Workshop.svelte:952](apps/aberp-ui/ui/src/routes/Workshop.svelte:952) | 18 events | ✅ pre-existing list |

The brief's deviation note: the brief proposed nested per-tile structs
(`WorkOrders { count_by_state, featured_list }` etc.). The shipped design
instead keeps the existing flat count fields and **adds sibling `*_rows`
fields** — a flatter wire shape that avoids renaming the count-shaped
`work_orders` field. Functionally identical to the brief's intent (count +
list per tile); the implementation just chose composition over nesting.

Visual weight, caps, mono-ref styling, dark-theme tokens, and the
"+N more" overflow are all present in CSS at [Workshop.svelte:1308](apps/aberp-ui/ui/src/routes/Workshop.svelte:1308)
onward (the `.ws-rows` block, explicitly tagged `S246 / PR-239`).

## Backend query path

`compute_workshop_dashboard` ([serve.rs:13632](apps/aberp/src/serve.rs:13632))
calls six row builders alongside the existing count queries, all on the
**same read-only tenant-scoped `Connection`** — no net-new DB roundtrips
beyond the capped slices, matching the brief's "don't add net-new DB
roundtrips" guidance:

- `build_work_order_rows` → `list_work_orders(.., LIMIT 5, 0)`
- `build_low_stock_rows`, `build_pending_qa_rows`
- `build_eligible_dispatch_rows`, `build_pending_dispatch_rows`
- `build_today_invoice_rows` (returns `(rows, total)` for the footer)

## Gates

Not re-run — this session adds **no code**, only this markdown findings doc.
The density feature has been green through CI since `PROD_v2.10.1` and is
part of the live `PROD_v2.27.12` baseline.

## Flagged conservative calls

1. **Refused the cut** (no `PROD_v2.27.13`) per HARD RULE #2 — the feature
   is shipped. Implementing the brief verbatim would have re-created
   existing fields and risked a wire-shape fork.
2. **Did not write the three requested `s320_*` backend regression tests.**
   They were scoped under "If implementing"; since the verdict is SHIPPED,
   adding them would itself require a code cut + CI + tag, contradicting the
   refusal. **However** — there is a genuine pre-existing gap: the six
   backend row builders (`build_work_order_rows` … `build_today_invoice_rows`)
   have **zero direct test coverage** (no `apps/aberp/tests/` file references
   them; only the vitest `workshop-mock-data.test.ts`, 18 tests, pins the
   *mock* shape). Flagged as a follow-up rather than bundled here.

## Branch

`session-320/pr-20-workshop-tv-density-verify-only` (doc-only, no tag).
