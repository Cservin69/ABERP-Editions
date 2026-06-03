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
    listStockMovements,
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
</dialog>

<style>
  .product-detail {
    width: min(900px, 90vw);
    max-height: 90vh;
    overflow-y: auto;
    padding: var(--space-4);
    border: none;
    border-radius: var(--radius-md, 8px);
    background: var(--color-surface, white);
    color: var(--color-text-strong, #111);
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
    color: var(--color-danger, #b00020);
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
    border-radius: 999px;
    font-size: 0.75rem;
    margin-left: var(--space-2);
  }
  .chip--low-stock {
    background: var(--color-danger-bg, #fdecec);
    color: var(--color-danger, #b00020);
    border: 1px solid var(--color-danger, #b00020);
  }

  .movement-form {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-2);
    align-items: end;
    padding: var(--space-3);
    background: var(--color-surface-raised, #f6f6f6);
    border-radius: var(--radius-sm, 4px);
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
    border: 1px solid var(--color-border, #ccc);
    border-radius: 4px;
    background: var(--color-surface, white);
    color: var(--color-text-strong, #111);
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
    border-bottom: 1px solid var(--color-border, #eee);
    text-align: left;
  }
  .ledger-table th.num,
  .ledger-table td.num {
    text-align: right;
  }
  .mono {
    font-family: var(--type-family-mono, monospace);
  }
  .negative {
    color: var(--color-danger, #b00020);
  }
  .page__primary {
    padding: 8px 14px;
    border-radius: 4px;
    background: var(--color-primary, #1769aa);
    color: var(--color-on-primary, white);
    border: none;
    cursor: pointer;
  }
  .page__primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .quiet-button {
    padding: 6px 10px;
    background: transparent;
    border: 1px solid var(--color-border, #ccc);
    border-radius: 4px;
    cursor: pointer;
  }
</style>
