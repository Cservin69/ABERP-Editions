<script lang="ts">
  // S231 / PR-227 / ADR-0061 — Stage 3 Phase γ Inventory v1 — the
  // per-product detail modal. Mirrors the InvoiceDetail modal shape:
  // a `<dialog>` overlay opened from the parent ProductsList row
  // ("Stock" button) that shows the product header (with the
  // low-stock chip + cached qty) + a "Stock movements" tab with the
  // ledger table and the manual adjustment form.
  //
  // The modal is keyed by `productId`; the parent re-renders
  // `<ProductDetail>` with a different id to swap products in place.
  // Closing the modal returns to the parent list, which refreshes via
  // its existing `loadProducts()` so the cached `stock_qty` on the row
  // reflects any movements posted while the modal was open.

  import { onMount } from "svelte";

  import {
    createStockMovement,
    getProduct,
    getProductBom,
    listProducts,
    listStockMovements,
    putProductBom,
    type BomLine,
    type Product,
    type StockMovement,
  } from "../lib/api";
  import { unitLabel } from "../lib/products";
  import {
    MANUAL_REASONS,
    formatQty,
    mintIdempotencyKey,
    reasonLabel,
  } from "../lib/stock-movements";
  import type { StockMovementReason } from "../lib/api";
  import {
    addBomRow,
    componentName,
    composeBomBody,
    emptyBomForm,
    formFromBomLines,
    isBomFormSubmittable,
    removeBomRow,
    updateBomRow,
    type BomFormState,
  } from "../lib/bom-form";
  import { formatTotal } from "../lib/format";

  type Props = {
    productId: string | null;
    onClose: () => void;
  };
  const { productId, onClose }: Props = $props();

  let product: Product | null = $state(null);
  let movements: StockMovement[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  // S232 — tab segmented control. "stock" is the default (Inventory v1
  // is the operator's daily driver — adding a recipe is a per-product
  // setup task). The active tab is local SPA state; the modal closing
  // resets to default on the next open via this var's declaration.
  let activeTab: "stock" | "bom" = $state("stock");

  // S232 — BOM authoring tab state.
  // - `bomLines` mirrors the GET /api/products/:id/bom response (the
  //   current active BOM). Used for the read-side table above the
  //   editor.
  // - `bomForm` is the operator-edited draft; folded from `bomLines`
  //   on every (re)load so the operator sees the current recipe
  //   pre-filled rather than typing it from scratch each time.
  // - `componentCatalog` is the full products list, used by the row's
  //   <select> to pick a component AND by the read-side table to
  //   resolve `component_id` → name.
  let bomLines: BomLine[] = $state([]);
  let bomForm: BomFormState = $state(emptyBomForm());
  let componentCatalog: Product[] = $state([]);
  let bomLoadState: "loading" | "loaded" | "error" = $state("loading");
  let bomLoadError: string | null = $state(null);
  let bomSaveState: "idle" | "saving" | "error" = $state("idle");
  let bomSaveError: string | null = $state(null);

  // Manual adjustment form state. `reason` defaults to Adjustment —
  // the only reason that accepts any sign per ADR-0061 §5, so the
  // operator's first stock-take entry does not bounce off the
  // reason-sign matrix.
  let formReason: StockMovementReason = $state("adjustment");
  let formQty: string = $state("");
  let formNotes: string = $state("");
  let submitState: "idle" | "submitting" | "error" = $state("idle");
  let submitError: string | null = $state(null);

  // ADR-0061 §6 — the dropdown surfaces only the manual-form reasons.
  // The other three (BomConsumption / WoCompletion / Dispatch) land
  // via upstream handlers; refusing them here AND at the backend is
  // defence in depth.
  const REASON_OPTIONS = MANUAL_REASONS.map((r) => ({
    value: r,
    label: reasonLabel(r, "en"),
    label_hu: reasonLabel(r, "hu"),
  }));

  onMount(() => {
    if (productId !== null) {
      void load(productId);
    }
  });

  // If the parent swaps the productId in place (e.g. operator picks
  // a different row), re-fetch.
  $effect(() => {
    const id = productId;
    if (id !== null) {
      void load(id);
    }
  });

  async function load(id: string) {
    loadState = "loading";
    loadError = null;
    try {
      const [p, m] = await Promise.all([getProduct(id), listStockMovements(id)]);
      product = p;
      movements = m;
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  // S232 — lazy-load the BOM data the first time the operator opens
  // the BOM tab (and on every subsequent open so a concurrent edit
  // somewhere else re-syncs). The component catalog rides in the same
  // fetch because the row picker + the read-side table both need it
  // to resolve `component_id` → name. `listProducts()` returns the
  // current product TOO (the row picker filters self-loops in the
  // template).
  async function loadBom(id: string) {
    bomLoadState = "loading";
    bomLoadError = null;
    try {
      const [lines, catalog] = await Promise.all([
        getProductBom(id),
        listProducts(),
      ]);
      bomLines = lines;
      bomForm = formFromBomLines(lines);
      componentCatalog = catalog;
      bomLoadState = "loaded";
    } catch (err: unknown) {
      bomLoadState = "error";
      bomLoadError = err instanceof Error ? err.message : String(err);
    }
  }

  // S232 — POST the operator-edited BOM. Backend semantics are full-
  // replace: every prior active row is soft-retired; the supplied list
  // becomes the new active set. On success, the response IS the new
  // active list — fold it back into the form so the operator sees the
  // saved state without a second round-trip.
  async function saveBom() {
    if (productId === null) return;
    bomSaveState = "saving";
    bomSaveError = null;
    try {
      const result = await putProductBom(productId, composeBomBody(bomForm));
      bomLines = result;
      bomForm = formFromBomLines(result);
      bomSaveState = "idle";
    } catch (err: unknown) {
      bomSaveState = "error";
      bomSaveError = err instanceof Error ? err.message : String(err);
    }
  }

  // S232 — auto-load BOM on tab open. `$effect` re-fires when either
  // `activeTab` or `productId` changes; the guard skips the initial
  // "stock" tab and the null-productId boot frame.
  $effect(() => {
    if (activeTab === "bom" && productId !== null) {
      void loadBom(productId);
    }
  });

  async function submit() {
    if (productId === null) return;
    submitState = "submitting";
    submitError = null;
    try {
      await createStockMovement(productId, {
        qty_delta: formQty.trim(),
        reason: formReason,
        idempotency_key: mintIdempotencyKey(),
        notes: formNotes.trim().length > 0 ? formNotes.trim() : undefined,
      });
      // Reset the form + re-fetch so the new row appears at the top
      // of the ledger and the header's cached stock_qty updates.
      formQty = "";
      formNotes = "";
      formReason = "adjustment";
      submitState = "idle";
      await load(productId);
    } catch (err: unknown) {
      submitState = "error";
      // The backend returns a structured `{ error: "..." }` body on
      // 400 / 409; Tauri's invoke surface unwraps that into the
      // error message. Display verbatim so the operator sees the
      // reason-sign matrix violation or the duplicate-idempotency-key
      // hint loud.
      submitError = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<dialog open class="product-detail" aria-labelledby="pd-title">
  <header class="product-detail__head">
    <h2 id="pd-title" class="product-detail__title">
      {#if product !== null}
        {product.name}
      {:else}
        Loading product…
      {/if}
    </h2>
    <button type="button" class="quiet-button" onclick={onClose}>Close</button>
  </header>

  {#if loadState === "loading"}
    <p class="product-detail__muted">Loading…</p>
  {:else if loadState === "error"}
    <p class="product-detail__error" role="alert">
      <strong>Could not load product.</strong>
      <span>{loadError ?? ""}</span>
    </p>
  {:else if product !== null}
    <section class="product-detail__summary" aria-label="Product summary">
      <dl class="kv">
        <dt>Unit</dt>
        <dd>{unitLabel(product.unit)}</dd>
        <dt>Unit price</dt>
        <dd class="mono">{formatTotal(product.unit_price_minor, product.currency)}</dd>
        <dt>Stock on hand</dt>
        <dd class="mono">
          {formatQty(product.stock_qty ?? "0")}
          {#if product.is_low_stock}
            <span class="chip chip--low-stock" aria-label="Stock below minimum"
              >Low stock</span
            >
          {/if}
        </dd>
        <dt>Min stock</dt>
        <dd class="mono">{formatQty(product.min_stock ?? "0")}</dd>
        {#if product.bin_location}
          <dt>Bin</dt>
          <dd class="mono">{product.bin_location}</dd>
        {/if}
        {#if product.last_movement_at}
          <dt>Last movement</dt>
          <dd class="mono">{product.last_movement_at}</dd>
        {/if}
      </dl>
    </section>

    <!-- S232 / PR-228 follow-up — tab segmented control. "Stock"
         keeps the daily-driver inventory ledger; "BOM" surfaces the
         per-product recipe authoring tab. The bilingual labels match
         the App.svelte Invoices tab pattern (HU on top, EN sub-label
         below). -->
    <div class="product-detail__tabs" role="tablist" aria-label="Product detail tabs">
      <button
        type="button"
        role="tab"
        class="product-detail__tab"
        class:product-detail__tab--active={activeTab === "stock"}
        aria-selected={activeTab === "stock"}
        onclick={() => (activeTab = "stock")}
      >
        <span class="product-detail__tab-label">Készlet</span>
        <span class="product-detail__tab-sub">Stock</span>
      </button>
      <button
        type="button"
        role="tab"
        class="product-detail__tab"
        class:product-detail__tab--active={activeTab === "bom"}
        aria-selected={activeTab === "bom"}
        onclick={() => (activeTab = "bom")}
        data-testid="product-detail-tab-bom"
      >
        <span class="product-detail__tab-label">Receptúra</span>
        <span class="product-detail__tab-sub">BOM</span>
      </button>
    </div>

    {#if activeTab === "stock"}
    <section class="product-detail__form" aria-labelledby="pd-form-title">
      <h3 id="pd-form-title">Post a stock movement</h3>
      <p class="product-detail__muted small">
        Manual adjustments only. Upstream-only reasons (BOM consumption,
        WO completion, Dispatch) land via their own handlers per
        ADR-0061 §6.
      </p>
      <form
        onsubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
        class="movement-form"
      >
        <label class="movement-form__field">
          <span>Reason</span>
          <select bind:value={formReason}>
            {#each REASON_OPTIONS as opt}
              <option value={opt.value}>{opt.label} ({opt.label_hu})</option>
            {/each}
          </select>
        </label>
        <label class="movement-form__field">
          <span>Qty delta (signed)</span>
          <input
            type="text"
            inputmode="decimal"
            bind:value={formQty}
            placeholder="e.g. 10 or -3.5"
            required
          />
        </label>
        <label class="movement-form__field movement-form__field--wide">
          <span>Notes (optional)</span>
          <input
            type="text"
            bind:value={formNotes}
            placeholder="Operator note (≤ 1024 chars)"
            maxlength="1024"
          />
        </label>
        <div class="movement-form__actions">
          <button
            type="submit"
            class="page__primary"
            disabled={submitState === "submitting" || formQty.trim().length === 0}
          >
            {submitState === "submitting" ? "Posting…" : "Post movement"}
          </button>
        </div>
        {#if submitError !== null}
          <p class="product-detail__error" role="alert">{submitError}</p>
        {/if}
      </form>
    </section>

    <section class="product-detail__ledger" aria-labelledby="pd-ledger-title">
      <h3 id="pd-ledger-title">Stock movements</h3>
      {#if movements.length === 0}
        <p class="product-detail__muted">No movements yet.</p>
      {:else}
        <table class="ledger-table">
          <thead>
            <tr>
              <th scope="col">When</th>
              <th scope="col" class="num">Qty Δ</th>
              <th scope="col">Reason</th>
              <th scope="col">Operator</th>
              <th scope="col">Notes</th>
            </tr>
          </thead>
          <tbody>
            {#each movements as m (m.movement_id)}
              <tr>
                <td class="mono">{m.at_iso8601}</td>
                <td class="num mono" class:negative={m.qty_delta.startsWith("-")}>
                  {formatQty(m.qty_delta)}
                </td>
                <td>{reasonLabel(m.reason, "en")}</td>
                <td class="mono">{m.operator}</td>
                <td>{m.notes ?? ""}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>
    {/if}

    {#if activeTab === "bom"}
      <!-- S232 / PR-228 follow-up — BOM authoring tab. Reads the
           current active BOM via GET /api/products/:id/bom and
           POSTs a new full-replace list on Save. Backend semantics:
           every prior active row is soft-retired in the same
           transaction so the next GET returns only the just-saved
           rows. -->
      <section class="product-detail__bom" aria-labelledby="pd-bom-title">
        <h3 id="pd-bom-title">Receptúra / Bill of materials</h3>
        <p class="product-detail__muted small">
          A komponensek és a darabonkénti mennyiségek mentésekor a
          korábbi receptúra automatikusan archiválódik. / Saving
          replaces the entire active recipe (prior rows are
          soft-retired).
        </p>

        {#if bomLoadState === "loading"}
          <p class="product-detail__muted">Betöltés… / Loading…</p>
        {:else if bomLoadState === "error"}
          <p class="product-detail__error" role="alert">
            <strong>Could not load BOM.</strong>
            <span>{bomLoadError ?? ""}</span>
          </p>
        {:else}
          {#if bomLines.length === 0}
            <p class="product-detail__muted">
              Még nincs aktív receptúra. / No active BOM yet.
            </p>
          {:else}
            <table class="ledger-table" aria-label="Active BOM">
              <thead>
                <tr>
                  <th scope="col">Komponens / Component</th>
                  <th scope="col" class="num">Mennyiség / Qty per unit</th>
                </tr>
              </thead>
              <tbody>
                {#each bomLines as line (line.bom_line_id)}
                  <tr>
                    <td>{componentName(line.component_id, componentCatalog)}</td>
                    <td class="num mono">{formatQty(line.qty_per_unit)}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          {/if}

          <form
            class="bom-form"
            onsubmit={(e) => {
              e.preventDefault();
              void saveBom();
            }}
          >
            <h4 class="bom-form__title">Szerkesztés / Edit</h4>
            {#each bomForm.rows as row, idx (idx)}
              <div class="bom-form__row">
                <label class="bom-form__field bom-form__field--grow">
                  <span>Komponens / Component</span>
                  <select
                    value={row.component_id}
                    onchange={(e) => {
                      bomForm = updateBomRow(bomForm, idx, {
                        component_id: (e.currentTarget as HTMLSelectElement).value,
                      });
                    }}
                    required
                  >
                    <option value="" disabled>— válassz / pick —</option>
                    {#each componentCatalog as p (p.id)}
                      {#if p.id !== productId}
                        <option value={p.id}>{p.name}</option>
                      {/if}
                    {/each}
                  </select>
                </label>
                <label class="bom-form__field">
                  <span>Mennyiség / Qty per unit</span>
                  <input
                    type="text"
                    inputmode="decimal"
                    value={row.qty_per_unit_input}
                    oninput={(e) => {
                      bomForm = updateBomRow(bomForm, idx, {
                        qty_per_unit_input: (e.currentTarget as HTMLInputElement)
                          .value,
                      });
                    }}
                    placeholder="e.g. 4 or 1.5"
                    required
                  />
                </label>
                <button
                  type="button"
                  class="quiet-button bom-form__remove"
                  onclick={() => (bomForm = removeBomRow(bomForm, idx))}
                  aria-label="Sor törlése / Remove row"
                >
                  ×
                </button>
              </div>
            {/each}

            <div class="bom-form__actions">
              <button
                type="button"
                class="quiet-button"
                onclick={() => (bomForm = addBomRow(bomForm))}
              >
                + Komponens hozzáadása / Add component
              </button>
              <button
                type="submit"
                class="page__primary"
                disabled={bomSaveState === "saving" || !isBomFormSubmittable(bomForm)}
              >
                {bomSaveState === "saving"
                  ? "Mentés… / Saving…"
                  : "Mentés / Save"}
              </button>
            </div>

            {#if bomSaveError !== null}
              <p class="product-detail__error" role="alert">{bomSaveError}</p>
            {/if}
          </form>
        {/if}
      </section>
    {/if}
  {/if}
</dialog>

<style>
  .product-detail {
    width: min(900px, 90vw);
    max-height: 90vh;
    overflow-y: auto;
    padding: var(--space-4);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-md);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
  }

  .product-detail__head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }

  .product-detail__title {
    margin: 0;
    font-size: var(--type-size-lg, 1.25rem);
    font-weight: 600;
  }

  .product-detail__muted {
    color: var(--color-text-secondary, #666);
    font-size: var(--type-size-sm, 0.9rem);
  }
  .product-detail__muted.small {
    font-size: 0.85rem;
    margin: 0 0 var(--space-2) 0;
  }

  .product-detail__error {
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm, 0.9rem);
  }

  .product-detail__summary {
    margin-bottom: var(--space-4);
  }

  .kv {
    display: grid;
    grid-template-columns: max-content 1fr;
    column-gap: var(--space-3);
    row-gap: var(--space-1);
    margin: 0;
  }
  .kv dt {
    color: var(--color-text-secondary, #666);
    font-weight: 500;
  }
  .kv dd {
    margin: 0;
  }

  .chip {
    display: inline-block;
    padding: 1px 8px;
    border-radius: var(--radius-pill);
    font-size: 0.75rem;
    margin-left: var(--space-2);
  }
  .chip--low-stock {
    background: var(--color-surface-raised);
    color: var(--color-signal-negative);
    border: 1px solid var(--color-signal-negative);
  }

  .movement-form {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-2);
    align-items: end;
    padding: var(--space-3);
    background: var(--color-surface-raised, #f6f6f6);
    border-radius: var(--radius-sm);
  }
  .movement-form__field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .movement-form__field--wide {
    grid-column: 1 / -1;
  }
  .movement-form__field span {
    font-size: 0.85rem;
    color: var(--color-text-secondary, #666);
  }
  .movement-form__field input,
  .movement-form__field select {
    padding: 6px 8px;
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }
  .movement-form__actions {
    grid-column: 1 / -1;
    display: flex;
    justify-content: flex-end;
  }

  .ledger-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.9rem;
  }
  .ledger-table th,
  .ledger-table td {
    padding: 6px 8px;
    border-bottom: 1px solid var(--color-surface-divider);
    text-align: left;
  }
  .ledger-table thead th {
    color: var(--color-text-secondary);
    font-weight: 500;
  }
  .ledger-table tbody td {
    color: var(--color-text-primary);
  }
  .ledger-table th.num,
  .ledger-table td.num {
    text-align: right;
  }
  .mono {
    font-family: var(--type-family-mono, monospace);
  }
  .negative {
    color: var(--color-signal-negative);
  }
  .page__primary {
    padding: 8px 14px;
    border-radius: var(--radius-sm);
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
    border: none;
    cursor: pointer;
    font-weight: 500;
  }
  .page__primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .quiet-button {
    padding: 6px 10px;
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    cursor: pointer;
  }
  .quiet-button:hover {
    color: var(--color-text-strong);
  }

  /* S232 / PR-228 follow-up — tab segmented control. Same visual
     posture as the App.svelte invoices tabs but scoped to the modal. */
  .product-detail__tabs {
    display: flex;
    gap: 4px;
    margin: var(--space-3) 0 var(--space-3) 0;
    border-bottom: 1px solid var(--color-surface-divider);
  }
  .product-detail__tab {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    padding: 6px 12px;
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    cursor: pointer;
    color: var(--color-text-secondary);
  }
  .product-detail__tab:hover {
    color: var(--color-text-strong);
  }
  .product-detail__tab--active {
    border-bottom-color: var(--color-signal-positive);
    color: var(--color-text-strong);
  }
  .product-detail__tab-label {
    font-weight: 600;
  }
  .product-detail__tab-sub {
    font-size: 0.75rem;
    color: var(--color-text-secondary);
  }

  /* S232 — BOM editor form layout. */
  .product-detail__bom {
    margin-top: var(--space-3);
  }
  .bom-form {
    margin-top: var(--space-3);
    padding: var(--space-3);
    background: var(--color-surface-raised, #f6f6f6);
    border-radius: var(--radius-sm);
  }
  .bom-form__title {
    margin: 0 0 var(--space-2) 0;
    font-size: 0.95rem;
    font-weight: 600;
  }
  .bom-form__row {
    display: flex;
    gap: var(--space-2);
    align-items: flex-end;
    margin-bottom: var(--space-2);
  }
  .bom-form__field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .bom-form__field--grow {
    flex: 1;
  }
  .bom-form__field span {
    font-size: 0.85rem;
    color: var(--color-text-secondary, #666);
  }
  .bom-form__field input,
  .bom-form__field select {
    padding: 6px 8px;
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }
  .bom-form__remove {
    align-self: flex-end;
    padding: 4px 10px;
    font-weight: bold;
  }
  .bom-form__actions {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
    margin-top: var(--space-2);
  }
</style>
