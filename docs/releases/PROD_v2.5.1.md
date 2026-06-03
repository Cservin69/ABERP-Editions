# PROD_v2.5.1 — dark-theme polish for S232 Work Orders + BOM tab

**Cutover date:** TBD (S232b / PR-228b; release branch push happens on
the operator's prod machine via `./run/release.sh PROD_v2.5.1`).
**Predecessor:** `PROD_v2.5.0` (S232 / PR-228 + bc805b1 follow-up,
Work Orders + 1-level BOM + linear Routing — Stage 3 Phase γ).
**Scope:** patch — pure SPA styling, zero functional change.

## Headline

**Work Orders + BOM tab now render in the dark theme.** S232's
SPA surfaces shipped with default browser styling: the Work Orders
list page, the "New work order" modal (worst offender — bright white
backdrop), the WO detail aside, the routing+BOM read-side tables, and
the BOM authoring tab on `ProductDetail.svelte` all displayed in
light/system theme against the otherwise-dark ABERP chrome.

PR-228b rewrites the relevant `<style>` blocks to use the dark tokens
defined in `apps/aberp-ui/ui/src/lib/tokens.css` — the same tokens
used by `PartnerForm.svelte` (canonical modal precedent) and
`IncomingInvoiceList.svelte` (canonical dense-table precedent).

## What changed

1. **`WorkOrdersList.svelte`** — full `<style>` block rewrite. Every
   hardcoded hex (`#ccc`, `#fff`, `#fafafa`, `#666`, `#eee`, etc.)
   replaced with the proper dark token. Native form controls (the
   modal's `<input>` / `<select>` / `<textarea>`) get explicit dark
   styling because browsers default them to system theme.
2. **`ProductDetail.svelte`** — surgical retirement of legacy token
   references that do not exist in `tokens.css` and were falling back
   to bright defaults. The affected lines were ALREADY broken since
   PR-227 (S231 Inventory v1), but the Stock modal had not yet been
   visually tested in dark theme. PR-228b's BOM tab inherits that
   broken cascade, so fixing the BOM tab in isolation would have left
   it sitting inside a bright-white dialog. The mechanical swaps:
   - `var(--color-surface, white)` → `var(--color-surface-base)`
   - `var(--color-border, #ccc/#eee)` → `var(--color-surface-divider)`
   - `var(--color-primary, #1769aa)` → `var(--color-signal-positive)`
     (primary button) or `var(--color-text-strong)`+
     `var(--color-signal-positive)` underline (active tab)
   - `var(--color-on-primary, white)` → `var(--color-surface-base)`
   - `var(--color-danger, #b00020)` → `var(--color-signal-negative)`
   - `var(--color-danger-bg, #fdecec)` → `var(--color-surface-raised)`

## Per-surface dark-token table

| Surface | Background | Border | Primary text |
|---------|-----------|--------|---------------|
| `.wo-page` | inherits `--color-surface-base` | — | `--color-text-primary` |
| `.wo-facet` (idle) | `--color-surface-raised` | `--color-surface-divider` | `--color-text-secondary` |
| `.wo-facet--active` | `--color-surface-raised` | `--color-text-muted` | `--color-text-strong` |
| `.wo-table` thead | `--color-surface-sunken` | `--color-surface-divider` | `--color-text-secondary` |
| `.wo-table` tbody | `--color-surface-sunken` (`:hover` → raised) | `--color-surface-divider` | `--color-text-primary` |
| `.wo-empty` | `--color-surface-raised` (dashed border) | `--color-surface-divider` | `--color-text-muted` |
| `.wo-detail` aside | `--color-surface-raised` | `--color-surface-divider` | `--color-text-primary` |
| `.wo-warnings` | `--color-surface-raised` + left-border `--color-signal-warning` | — | `--color-text-primary` |
| `.wo-modal` backdrop | `rgba(0,0,0,0.5)` | — | — |
| `.wo-modal__body` | `--color-surface-base` | `--color-surface-divider` | `--color-text-primary` |
| `.wo-modal__body label input/select/textarea` | `--color-surface-base` | `--color-surface-divider` | `--color-text-strong` (mono) |
| `.wo-modal__actions` Cancel | `--color-surface-raised` | `--color-surface-divider` | `--color-text-secondary` |
| `.wo-modal__actions` Save | `--color-signal-positive` | `--color-signal-positive` | `--color-surface-base` |
| `.product-detail` (S231 dialog) | `--color-surface-base` | `--color-surface-divider` | `--color-text-primary` |
| `.product-detail__tab--active` | transparent | bottom `--color-signal-positive` | `--color-text-strong` |
| `.bom-form` | `--color-surface-raised` (already correct) | — | inherits primary |
| `.bom-form__field input/select` | `--color-surface-base` | `--color-surface-divider` | `--color-text-strong` (mono) |

Each row above is "uses dark tokens because PartnerForm/IncomingInvoiceList
ships the same pattern" — these are the two canonical references for
SPA chrome in this repo.

## Files touched

- `apps/aberp-ui/ui/src/routes/WorkOrdersList.svelte` — `<style>` block
  full rewrite. +319 / −45 lines (net +274). DOM unchanged.
- `apps/aberp-ui/ui/src/routes/ProductDetail.svelte` — 7 surgical edits
  in the `<style>` block to retire broken legacy token names. +41 / −24
  lines (net +17). DOM unchanged.

## Out of scope (deliberately)

- **No backend changes.** Zero Rust touched.
- **No DOM changes.** Class names, ARIA roles, event handlers, all
  preserved. Existing SPA tests (886/886) pass without modification.
- **No new tokens introduced.** Every replacement uses a token already
  defined in `apps/aberp-ui/ui/src/lib/tokens.css`.
- **`ExtNavPartnerPickerModal.svelte`** (PR-217 / S220) uses the same
  broken-legacy-token shorthand (`var(--surface, white)`). NOT touched
  in PR-228b — it is not a S232 surface and Ervin did not call it out.
  Flagged in [[spa-dark-theme-default]] for a future polish PR.

## Persisted as a rule

`[[spa-dark-theme-default]]` is now an auto-memory entry under
`.claude/projects/.../memory/feedback_spa_dark_theme_default.md`. Future
SPA work in this repo will pick it up automatically; every new
component is expected to use dark tokens from day one.

## Breaking changes

**NONE.** Pure CSS polish. Identical DOM, identical behavior.

## Verification

- `npm run test` (apps/aberp-ui/ui): 42 files / 886 tests passed.
- `npm run check` (svelte-check): 2 pre-existing errors in
  `src/lib/hygiene-clickthrough.test.ts` (confirmed identical against
  `main`); 0 new errors.
- `npm run build`: succeeded; 207 modules transformed.
- Backend gates: not run — no Rust touched.

## Rollback

`./run/upgrade_prod.sh PROD_v2.5.0` restores the prior release.
PR-228b is CSS-only; no schema, no audit kind, no migration.
