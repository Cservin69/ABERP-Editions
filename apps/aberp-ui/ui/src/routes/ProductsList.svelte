<script lang="ts">
  // PR-91 — Products master-data management screen. Mirrors the
  // PartnersList layout (PR-54): list table + "+ New product" header
  // button + per-row Edit / Delete + a client-side filter on name.
  //
  // The line-editor integration ("pick a product → autofill an invoice
  // line") is OUT OF SCOPE for this PR; see the PR-91 handoff. This
  // screen is the master-data CRUD that the future autofill draws
  // from.

  import { onDestroy, onMount } from "svelte";

  import { deleteProduct, listProducts, type Product } from "../lib/api";
  import { formatTotal } from "../lib/format";
  import {
    makeHotkeyParserState,
    nextRowIndex,
    parseHotkey,
  } from "../lib/keyboard-nav";
  // PR-181 / session-181 — persist the quick-filter needle to
  // localStorage. PR-194 / session-194 — extended to persist sort +
  // Unit + Currency facets. Seeded synchronously at component init
  // (before first render) so the rendered list reflects persisted
  // state without a flash of the unsorted / unfiltered set.
  import {
    loadProductListPrefs,
    saveProductListPrefs,
  } from "../lib/product-list-persistence";
  import {
    EMPTY_PRODUCT_FILTER,
    compareProducts,
    filterProductsWith,
    isProductFilterEmpty,
    unitFacetKey,
    unitLabel,
    type CurrencyFacet,
    type ProductFilterSpec,
    type ProductSortKey,
    type UnitFacet,
  } from "../lib/products";
  import type { SortDir } from "../lib/list-sort";
  // PR-193 / session-193 — CSV export of the currently-displayed
  // (filtered) product set. Tier-4 "invisible excellence" lift.
  import {
    composeCsv,
    csvFilenameTimestamp,
    downloadCsv,
  } from "../lib/csv-export";
  import ProductForm from "./ProductForm.svelte";

  let rows: Product[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  // PR-194 / session-194 — seed `filter` + `sort` from `localStorage`
  // so a reload restores the operator's last view. Default fallback
  // on every failure path.
  const initialPrefs = loadProductListPrefs();

  let filter: ProductFilterSpec = $state({ ...initialPrefs.filter });
  let sort: { key: ProductSortKey | null; dir: SortDir } = $state({
    ...initialPrefs.sort,
  });

  // Modal state: `null` = closed, `"new"` = create-mode, `Product`
  // = edit-mode pre-filled.
  let modalState: "new" | Product | null = $state(null);

  // Inline confirm state for delete (id pending confirmation).
  let confirmDeleteId: string | null = $state(null);
  let deleteError: string | null = $state(null);

  // PR-194 — auto-populated Unit facet vocabulary, derived from the
  // currently-loaded rows. The dropdown shows "All" + each distinct
  // unit (operator-visible label, stable `kind:value` key). The set
  // is rebuilt on every refresh; an `Own:` label that's been deleted
  // disappears from the dropdown automatically. Order: Hungarian
  // locale-aware on the visible label so the dropdown reads in
  // operator-natural order.
  let unitFacetOptions = $derived.by(() => {
    const seen = new Map<string, string>();
    for (const p of rows) {
      const key = unitFacetKey(p.unit);
      if (!seen.has(key)) seen.set(key, unitLabel(p.unit));
    }
    return Array.from(seen.entries())
      .map(([key, label]) => ({ key, label }))
      .sort((a, b) => a.label.localeCompare(b.label, "hu"));
  });

  // PR-194 — defensive: if the persisted Unit facet does not match any
  // currently-loaded row, reset to "All" once rows arrive. Without this
  // the operator sees an "inactive" filter (nothing renders) that the
  // dropdown can't unwind because the picked value isn't an option.
  $effect(() => {
    if (
      filter.unit !== "All" &&
      rows.length > 0 &&
      !unitFacetOptions.some((opt) => opt.key === filter.unit)
    ) {
      filter = { ...filter, unit: "All" };
    }
  });

  // PR-194 — filter → sort composition. `sort.key === null` keeps the
  // backend's natural ordering (name ASC from `list_products`); a
  // non-null key sorts client-side via `compareProducts`. The slice()
  // defends the in-place Array.prototype.sort from leaking back into
  // `rows`.
  let filtered = $derived(
    sort.key === null
      ? filterProductsWith(rows, filter)
      : filterProductsWith(rows, filter)
          .slice()
          .sort((a, b) => compareProducts(a, b, sort.key as ProductSortKey, sort.dir)),
  );

  // Keyboard-nav state mirrors PartnersList per PR-68 — `/` focuses
  // the search input, `j`/`k` walk filtered rows, Enter opens the
  // focused row's edit modal.
  let focusedRowIndex: number = $state(-1);
  let hintsVisible: boolean = $state(true);
  let searchInputEl: HTMLInputElement | null = $state(null);
  const parserState = makeHotkeyParserState();

  $effect(() => {
    if (filtered.length === 0) {
      focusedRowIndex = -1;
    } else if (focusedRowIndex >= filtered.length) {
      focusedRowIndex = filtered.length - 1;
    }
  });

  // PR-181 — persist the needle on every mutation.
  // PR-194 — extended to persist sort + Unit + Currency facets.
  $effect(() => {
    saveProductListPrefs({
      sort: { key: sort.key, dir: sort.dir },
      filter: {
        needle: filter.needle,
        unit: filter.unit,
        currency: filter.currency,
      },
    });
  });

  onMount(() => {
    void loadProducts();
    window.addEventListener("keydown", handleKeydown);
  });

  onDestroy(() => {
    window.removeEventListener("keydown", handleKeydown);
  });

  function handleKeydown(event: KeyboardEvent) {
    if (modalState !== null) return;
    const hotkey = parseHotkey(event, parserState);
    if (hotkey === null) return;
    switch (hotkey.kind) {
      case "focus-search":
        event.preventDefault();
        searchInputEl?.focus();
        searchInputEl?.select();
        return;
      case "blur-or-clear":
        if (event.target === searchInputEl) {
          if (filter.needle.length > 0) {
            filter = { ...filter, needle: "" };
          } else {
            searchInputEl?.blur();
          }
        }
        return;
      case "row-down":
        event.preventDefault();
        focusedRowIndex = nextRowIndex(focusedRowIndex, 1, filtered.length);
        return;
      case "row-up":
        event.preventDefault();
        focusedRowIndex = nextRowIndex(focusedRowIndex, -1, filtered.length);
        return;
      case "row-top":
        event.preventDefault();
        focusedRowIndex = filtered.length > 0 ? 0 : -1;
        return;
      case "row-bottom":
        event.preventDefault();
        focusedRowIndex = filtered.length > 0 ? filtered.length - 1 : -1;
        return;
      case "row-open":
        // PR-194 — Enter-on-button suppression: when a sort header
        // (or any other <button>) holds keyboard focus, let the
        // browser's native click handler fire instead of double-
        // firing the row-open.
        if (
          event.target instanceof HTMLElement &&
          event.target.tagName === "BUTTON"
        ) {
          return;
        }
        if (focusedRowIndex >= 0 && focusedRowIndex < filtered.length) {
          event.preventDefault();
          openEdit(filtered[focusedRowIndex]);
        }
        return;
      case "toggle-hints":
        event.preventDefault();
        hintsVisible = !hintsVisible;
        return;
    }
  }

  async function loadProducts() {
    loadState = "loading";
    loadError = null;
    try {
      rows = await listProducts();
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function openCreate() {
    modalState = "new";
  }

  function openEdit(product: Product) {
    modalState = product;
  }

  function closeModal() {
    modalState = null;
  }

  async function onSaved() {
    modalState = null;
    await loadProducts();
  }

  function requestDelete(productId: string) {
    confirmDeleteId = productId;
    deleteError = null;
  }

  function cancelDelete() {
    confirmDeleteId = null;
    deleteError = null;
  }

  async function confirmDelete(productId: string) {
    deleteError = null;
    try {
      await deleteProduct(productId);
      confirmDeleteId = null;
      await loadProducts();
    } catch (err: unknown) {
      deleteError = err instanceof Error ? err.message : String(err);
    }
  }

  // PR-194 / session-194 — three-click sort cycle. Same posture as
  // PartnersList::onSortClick / InvoiceList::onSortClick.
  function onSortClick(key: ProductSortKey) {
    if (sort.key !== key) {
      sort = { key, dir: "asc" };
      return;
    }
    if (sort.dir === "asc") {
      sort = { key, dir: "desc" };
      return;
    }
    sort = { key: null, dir: "asc" };
  }

  function sortIndicator(key: ProductSortKey): string {
    if (sort.key !== key) return "";
    return sort.dir === "asc" ? "▲" : "▼";
  }

  function ariaSortFor(key: ProductSortKey): "ascending" | "descending" | "none" {
    if (sort.key !== key) return "none";
    return sort.dir === "asc" ? "ascending" : "descending";
  }

  // PR-194 — closed-vocab Currency facet picker values. HUF + EUR
  // mirror the `Currency` union (ADR-0037 §3); widening to a third
  // lifts here when `Currency` widens.
  const CURRENCY_FACET_OPTIONS: readonly { value: CurrencyFacet; label: string }[] = [
    { value: "All", label: "All" },
    { value: "HUF", label: "HUF" },
    { value: "EUR", label: "EUR" },
  ];

  // PR-193 / session-193 — CSV export of the currently-filtered
  // product set. Columns match the screen (Name / Unit / Unit price)
  // with the currency as its own column (the screen renders it next
  // to the price via `formatTotal`) so a bookkeeper sorting by price
  // can also sort by currency. The price is emitted in MAJOR units
  // (HUF passes through; EUR cents → euros) to match the operator's
  // mental model from the screen.
  function exportCsv() {
    const headers = ["Name", "Unit", "Currency", "Unit price"];
    const rowsOut: unknown[][] = filtered.map((p) => {
      const priceMajor =
        p.currency === "EUR" ? p.unit_price_minor / 100 : p.unit_price_minor;
      return [p.name, unitLabel(p.unit), p.currency, priceMajor];
    });
    const csv = composeCsv(headers, rowsOut);
    const filename = `aberp-products-${csvFilenameTimestamp()}.csv`;
    downloadCsv(filename, csv);
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Products</h2>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New product
      </button>
    </div>
    <p class="page__lede">
      Catalog of saleable items: name, unit of measure (NAV-aligned),
      currency, and set price. The unit-of-measure dropdown matches
      NAV's enum where one exists (PIECE, KILOGRAM, DAY, …); any
      custom label (e.g. <code>liter@15C</code>) lives under
      <em>Egyéb (Own)</em>.
    </p>
  </header>

  <div class="page__toolbar">
    <label class="page__search">
      <span class="visually-hidden">Filter products</span>
      <input
        bind:this={searchInputEl}
        type="search"
        value={filter.needle}
        oninput={(e) =>
          (filter = {
            ...filter,
            needle: (e.currentTarget as HTMLInputElement).value,
          })}
        placeholder="Filter by name… (press /)"
        autocomplete="off"
        spellcheck="false"
      />
    </label>
    <!-- PR-194 / session-194 — Unit facet picker. Auto-populated from
         the currently-loaded rows. -->
    <label class="filter">
      <span class="filter-label">Unit</span>
      <select
        value={filter.unit}
        onchange={(e) =>
          (filter = {
            ...filter,
            unit: (e.currentTarget as HTMLSelectElement).value as UnitFacet,
          })}
        aria-label="Filter products by unit"
        disabled={unitFacetOptions.length === 0}
      >
        <option value="All">All</option>
        {#each unitFacetOptions as option (option.key)}
          <option value={option.key}>{option.label}</option>
        {/each}
      </select>
    </label>
    <!-- PR-194 / session-194 — Currency facet picker. Closed-vocab
         (HUF + EUR per ADR-0037 §3). -->
    <label class="filter">
      <span class="filter-label">Currency</span>
      <select
        value={filter.currency}
        onchange={(e) =>
          (filter = {
            ...filter,
            currency: (e.currentTarget as HTMLSelectElement).value as CurrencyFacet,
          })}
        aria-label="Filter products by currency"
      >
        {#each CURRENCY_FACET_OPTIONS as option (option.value)}
          <option value={option.value}>{option.label}</option>
        {/each}
      </select>
    </label>
    <!-- PR-193 / session-193 — CSV export of the currently-filtered
         rows. Disabled when nothing would be exported. -->
    <button
      type="button"
      class="quiet-button"
      onclick={exportCsv}
      disabled={filtered.length === 0}
      title="Export the currently displayed products to a CSV file"
    >
      Export CSV
    </button>
  </div>

  {#if loadState === "loading"}
    <p class="page__muted">Loading…</p>
  {:else if loadState === "error"}
    <div class="page__error" role="alert">
      <strong>Could not load products.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else if rows.length === 0}
    <div class="page__empty">
      <p>No products yet. Add your first.</p>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New product
      </button>
    </div>
  {:else if filtered.length === 0}
    <p class="page__muted">
      No product matches the current filter.
      {#if !isProductFilterEmpty(filter)}
        <button
          type="button"
          class="quiet-button clear-filters"
          onclick={() => (filter = { ...EMPTY_PRODUCT_FILTER })}
        >
          Clear filters
        </button>
      {/if}
    </p>
  {:else}
    <table class="products-table">
      <thead>
        <tr>
          <!-- PR-194 / session-194 — sortable headers; same three-cycle
               pattern as InvoiceList / PartnersList. -->
          <th scope="col" aria-sort={ariaSortFor("name")}>
            <button
              type="button"
              class="sort-header"
              onclick={() => onSortClick("name")}
            >
              <span>Name</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("name")}</span
              >
            </button>
          </th>
          <th scope="col" aria-sort={ariaSortFor("unit")}>
            <button
              type="button"
              class="sort-header"
              onclick={() => onSortClick("unit")}
            >
              <span>Unit</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("unit")}</span
              >
            </button>
          </th>
          <th scope="col" aria-sort={ariaSortFor("currency")}>
            <button
              type="button"
              class="sort-header"
              onclick={() => onSortClick("currency")}
            >
              <span>Currency</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("currency")}</span
              >
            </button>
          </th>
          <th scope="col" class="num" aria-sort={ariaSortFor("price")}>
            <button
              type="button"
              class="sort-header right"
              onclick={() => onSortClick("price")}
            >
              <span>Unit price</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("price")}</span
              >
            </button>
          </th>
          <th scope="col" class="actions-header">
            <span class="visually-hidden">Actions</span>
          </th>
        </tr>
      </thead>
      <tbody>
        {#each filtered as product, rowIndex (product.id)}
          {@const isKeyboardFocused = rowIndex === focusedRowIndex}
          <tr class:row-focused={isKeyboardFocused}>
            <td>{product.name}</td>
            <td>
              <span class="unit-chip" data-kind={product.unit.kind}>
                {unitLabel(product.unit)}
              </span>
            </td>
            <td class="mono">{product.currency}</td>
            <td class="num mono">
              {formatTotal(product.unit_price_minor, product.currency)}
            </td>
            <td class="actions">
              {#if confirmDeleteId === product.id}
                <div class="confirm">
                  <span class="confirm__text">
                    Soft-delete <strong>{product.name}</strong>? It stays
                    in the database; future invoices that reference it
                    still resolve.
                  </span>
                  <div class="confirm__buttons">
                    <button
                      type="button"
                      class="quiet-button"
                      onclick={cancelDelete}
                    >
                      Cancel
                    </button>
                    <button
                      type="button"
                      class="quiet-button danger"
                      onclick={() => void confirmDelete(product.id)}
                    >
                      Delete
                    </button>
                  </div>
                  {#if deleteError !== null}
                    <p class="confirm__error" role="alert">{deleteError}</p>
                  {/if}
                </div>
              {:else}
                <button
                  type="button"
                  class="quiet-button"
                  onclick={() => openEdit(product)}
                >
                  Edit
                </button>
                <button
                  type="button"
                  class="quiet-button"
                  onclick={() => requestDelete(product.id)}
                >
                  Delete
                </button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if hintsVisible}
    <p class="keyboard-hints" aria-hidden="true">
      Press <kbd>/</kbd> to search • <kbd>j</kbd>/<kbd>k</kbd> to navigate •
      <kbd>Enter</kbd> to edit • <kbd>?</kbd> to hide
    </p>
  {/if}
</section>

{#if modalState !== null}
  <ProductForm
    product={modalState === "new" ? null : modalState}
    onSaved={onSaved}
    onClose={closeModal}
  />
{/if}

<style>
  .page {
    max-width: 1100px;
    margin: 0 auto;
  }

  .page__head {
    margin-bottom: var(--space-4);
  }

  .page__head-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
  }

  .page__title {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-lg);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .page__lede {
    margin: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
    max-width: 70ch;
  }

  .page__lede code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
    background: var(--color-surface-raised);
    padding: 0 4px;
    border-radius: 3px;
  }

  .page__toolbar {
    margin-bottom: var(--space-3);
    /* PR-193 / session-193 — flex so the new "Export CSV" button
     * sits next to the search input with a consistent gap. The
     * pre-PR-193 toolbar held only the search input. */
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }

  .page__search input {
    width: 320px;
    max-width: 100%;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    border-radius: 4px;
  }

  .page__muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }

  .page__empty {
    padding: var(--space-5);
    border: 1px dashed var(--color-surface-divider);
    background: var(--color-surface-raised);
    text-align: center;
    color: var(--color-text-secondary);
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-3);
  }

  .page__primary {
    padding: var(--space-2) var(--space-4);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: 4px;
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .page__primary:hover {
    opacity: 0.9;
  }

  .page__error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    font-size: var(--type-size-sm);
  }

  .page__error-detail {
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .products-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }

  .products-table th,
  .products-table td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
    vertical-align: top;
  }

  .products-table th {
    color: var(--color-text-secondary);
    font-weight: 500;
    background: var(--color-surface-raised);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: var(--type-size-xs);
  }

  /* PR-194 / session-194 — sortable column header buttons. Mirror of
   * InvoiceList / PartnersList. */
  .sort-header {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: inherit;
    text-transform: inherit;
    letter-spacing: inherit;
    text-align: inherit;
    cursor: pointer;
    display: inline-flex;
    align-items: baseline;
    gap: var(--space-1);
  }

  .sort-header.right {
    justify-content: flex-end;
    width: 100%;
  }

  .sort-header:hover,
  .sort-header:focus-visible {
    color: var(--color-text-strong);
  }

  .sort-header:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  .sort-indicator {
    display: inline-block;
    min-width: 0.75em;
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  /* PR-194 — Unit + Currency facet chips. */
  .filter {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }

  .filter-label {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
  }

  .filter select {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .filter select:hover:not(:disabled) {
    border-color: var(--color-text-muted);
  }

  .filter select:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .clear-filters {
    margin-left: var(--space-2);
  }

  .products-table td.mono {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  .products-table th.num,
  .products-table td.num {
    text-align: right;
  }

  .actions-header {
    width: 1%;
  }

  .actions {
    white-space: nowrap;
    display: flex;
    gap: var(--space-2);
  }

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    border-radius: 4px;
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quiet-button.danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .unit-chip {
    display: inline-block;
    padding: 0 var(--space-2);
    border-radius: 12px;
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-size: var(--type-size-xs);
    font-weight: 500;
  }

  /* `Own` is the escape hatch — visually distinguished so the
   * operator can scan for "which products use a custom unit?". */
  .unit-chip[data-kind="Own"] {
    border-color: var(--color-signal-muted);
    border-style: dashed;
  }

  .confirm {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-2);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    max-width: 360px;
    white-space: normal;
  }

  .confirm__text {
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .confirm__text strong {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }

  .confirm__buttons {
    display: flex;
    gap: var(--space-2);
  }

  .confirm__error {
    margin: 0;
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    word-break: break-word;
  }

  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }

  .products-table tbody tr.row-focused {
    background: var(--color-surface-raised);
    outline: 1px solid var(--color-text-muted);
    outline-offset: -1px;
  }

  .keyboard-hints {
    margin: var(--space-3) 0 0 0;
    text-align: right;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-body);
  }

  .keyboard-hints kbd {
    display: inline-block;
    padding: 0 var(--space-1);
    border: 1px solid var(--color-surface-divider);
    border-radius: 2px;
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    line-height: 1.4;
  }
</style>
