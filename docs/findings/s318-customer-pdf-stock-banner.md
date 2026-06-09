# S318 — EVE addendum 2, customer-facing PDF stock-alert banner

**Session:** S318 / PR-18
**Branch:** `session-318/pr-18-addendum2-customer-pdf-banner`
**Date:** 2026-06-09
**Status of the feature after this session:** PDF *capability* shipped ABERP-side; end-to-end delivery to the customer is **BLOCKED on a storefront change** (documented below).

---

## 1. Ownership verdict — ABERP renders the customer-facing PDF

Step-0 of the brief asked whether the customer-facing quote PDF is rendered by
ABERP or by the storefront (ADR-0009 storefront-as-queue). Chain of reads:

1. `apps/aberp/src/quote_pricing_pipeline.rs:714` — the pricing pipeline's
   `advance_render` step calls `aberp_quote_pdf::render(&inputs)` and writes the
   bytes to `<artifact_dir>/<quote_id>/priced.pdf`.
2. `apps/aberp/src/quote_pricing_pipeline.rs:921` — `post_priced_writeback`
   POSTs those bytes as the `pdf` multipart field to the storefront
   `POST /api/quotes/{id}/priced`.
3. storefront `src/routes/api/quotes/[id]/priced/+server.ts` — validates and
   stores the PDF via `writePricedPdfAtomic(id, pdfBytes)`.
4. storefront `src/routes/api/quotes/[id]/pdf/+server.ts` — streams that exact
   file to the **customer**, gated by the same HMAC `?t=` token that protects
   `/q/{id}`, with `content-disposition: inline`.
5. storefront `src/routes/q/[id]/+page.svelte` — the customer's quote view; the
   PDF is offered there.

**Verdict: ABERP owns the customer PDF.** There is no storefront-side PDF
renderer — the storefront only stores and serves the ABERP artifact.

---

## 2. What shipped this session (ABERP-side)

The audit's literal "missing half" — the PDF renderer had zero knowledge of
`stock_alert` — is closed:

- `crates/aberp-quote-pdf/src/lib.rs`
  - `QuoteInputs` gains `stock_alert: bool` (defaults `false`, back-compat).
  - New constants `STOCK_ALERT_BANNER_HU` / `STOCK_ALERT_BANNER_EN`.
  - When `stock_alert == true`, a **red stock-status band** is drawn at the
    **top of the page** (before the customer/pricing blocks, so the customer
    sees it first): red rules bracketing a bold HU line + an EN line. Bilingual
    to mirror the storefront customer HTML banner.
  - `winansi_byte_for_char` gains the em-dash mapping (`U+2014 → 0x97`) so the
    banner's `—` renders correctly (also fixes the latent footer "Indicative
    quote —" which previously degraded to `?`).
  - New helpers `push_text_red` / `push_rule_red`.
  - Regression tests `s318_addendum2_banner_renders_when_stock_alert_true` and
    `s318_addendum2_no_banner_when_stock_alert_false`.
- `apps/aberp/src/quote_pricing_pipeline.rs`
  - The `advance_render` `QuoteInputs` literal sets `stock_alert: false` (with a
    comment) — first-render is always pre-acceptance, so the flag is necessarily
    false there.
  - The `build_priced_multipart` `"stock_alert": false` meta line gains a comment
    pointing here.

---

## 3. Why ABERP carries no re-render / re-post seam (the blocked half)

The brief's plan C ("emit `quote.pdf_rerender_requested` inside
`quote_stock_alert.rs::recompute`, wire a re-render task") rests on two
assumptions that the code contradicts:

1. **`recompute_stock_alert` is a pure function** (`apps/aberp/src/quote_stock_alert.rs`)
   — no side effects, no persistence, no audit emit. The FALSE→TRUE transition is
   detected **read-side**, on the operator's Quotes-list load, in
   `apps/aberp/src/quote_intake_query.rs::list_quote_intake_rows` (which calls
   `flip_stock_alert_to_true`) and `apps/aberp/src/serve.rs` (which emits the one
   `QuoteStockAlertTriggered` audit entry). There is no daemon and nothing to
   "emit inside recompute."

2. **`stock_alert` lives in the `quote_intake_log` subsystem, not
   `quote_pricing_jobs`.** The pricing pipeline that renders/posts `priced.pdf`
   has no `stock_alert` column. The two subsystems are joined only by `quote_id`.

3. **The pipeline posts the PDF exactly once, at pricing time**, when the
   downgrade has not happened yet (the snapshot/compare that fires `stock_alert`
   only exists after the customer accepts — `priced → accepted`, storefront-side).

4. **The customer's storefront `stock_alert` is set ONLY by ABERP's `/priced`
   meta**, which is hardcoded `false`. There is no storefront-side recompute.
   Consequence: the already-shipped customer HTML banner
   (`q/[id]/+page.svelte:80`) is **also dead today** — it can never become `true`
   until ABERP re-posts `stock_alert: true`.

5. **The re-post is blocked by the storefront `/priced` endpoint.** Per ADR-0004
   its idempotency/state machine is:
   - `quoted` + same `feature_graph_hash` → `200 { idempotent: true }`, **PDF is
     NOT overwritten**.
   - `quoted` + different hash → `409 already_priced_with_different_hash`.
   - post-acceptance status (`accepted`/terminal) → `409`.

   A re-render carries the *same* `feature_graph_hash` (the geometry is
   unchanged — only the stock flag moved), so a re-post is swallowed as an
   idempotent no-op and the stale (banner-less) PDF stays in front of the
   customer. Re-rendering ABERP-side without changing the storefront would
   therefore **silently fail to update the customer** — exactly the
   "completed successfully but did nothing" class of bug CLAUDE.md rule 12
   forbids.

For these reasons this session did **not** add `EventKind::QuotePdfRerenderRequested`
(a no-producer event would be speculative, CLAUDE.md rule 13) nor a re-render
trigger that cannot deliver. The PDF capability is landed as a utility cut
(matching the S266/S268/S269/S270 "capability first, wire later" convention);
the wiring lands once the storefront accepts a stock-alert re-render.

---

## 4. Storefront follow-up brief (next session, abenerp.com repo)

**Goal.** Make the customer actually see the addendum-2 stock-alert — on both the
HTML view (already coded, currently dead) and the PDF (capability now exists
ABERP-side) — by allowing ABERP to push a stock-status downgrade after the
initial pricing.

**Scope.** The storefront `POST /api/quotes/[id]/priced` endpoint
(`src/routes/api/quotes/[id]/priced/+server.ts`) must accept a *stock-alert
re-render*: a re-post that carries the **same `feature_graph_hash`** but a
**`stock_alert: true`** meta and a fresh PDF, for a quote in `quoted` **or**
post-acceptance-but-non-terminal state. Today that path returns
`200 { idempotent: true }` (same hash) without overwriting the stored PDF or
flipping `pricing.stock_alert`, so the downgrade never reaches the customer. The
follow-up should: (a) when the incoming `stock_alert` is `true` and the stored
one is `false`, overwrite `priced.pdf` and set `pricing.stock_alert = true` even
on a same-hash post (the hash guards *geometry/pricing* identity, not the
stock-status overlay); (b) keep the existing different-hash and terminal-state
409s; (c) re-send the customer "stock changed" notification at most once
(sticky, mirroring the ABERP `recompute_stock_alert` sticky semantics).

**Then the ABERP side** gets a small companion follow-up: a producer that, on the
read-side FALSE→TRUE transition already detected in
`quote_intake_query::list_quote_intake_rows` / `serve.rs`, re-renders `priced.pdf`
with `stock_alert: true` (the capability shipped in this PR) and re-POSTs it via
`post_priced_writeback` with `stock_alert: true` in the meta — at which point the
audit event `quote.pdf_rerender_requested` becomes worth adding, because it will
finally have a real producer and a delivery path.

**File:line references**
- ABERP render: `apps/aberp/src/quote_pricing_pipeline.rs:700` (QuoteInputs),
  `:1606` (`build_priced_multipart` meta), `:921` (`post_priced_writeback`).
- ABERP PDF crate: `crates/aberp-quote-pdf/src/lib.rs` (`QuoteInputs.stock_alert`,
  `STOCK_ALERT_BANNER_*`, stock-alert band in `build_content`).
- ABERP transition detection: `apps/aberp/src/quote_stock_alert.rs`,
  `apps/aberp/src/quote_intake_query.rs` (`flip_stock_alert_to_true`),
  `apps/aberp/src/serve.rs` (`QuoteStockAlertTriggered` emit).
- storefront ingest: `src/routes/api/quotes/[id]/priced/+server.ts`
  (`validateMeta` already requires `stock_alert: boolean`; `QuotePricing.stock_alert`
  in `src/lib/server/quote-store.ts`).
- storefront customer view: `src/routes/q/[id]/+page.svelte:80`,
  `src/routes/q/[id]/+page.server.ts:53`; PDF serve
  `src/routes/api/quotes/[id]/pdf/+server.ts`.
