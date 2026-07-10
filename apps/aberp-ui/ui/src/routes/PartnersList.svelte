<script lang="ts">
  // PR-54 / session-74 — Partners management screen.
  //
  // Operator workflow:
  //   1. Open #/partners. Page lists every active partner.
  //   2. Click "+ New partner" or "Edit" → PartnerForm modal opens.
  //   3. Click "Delete" on a row → inline confirm → soft-delete; row
  //      disappears from the list (still in DB per A182).
  //   4. Type in the search box → client-side filter (no backend
  //      roundtrip — admin browsing is the use case, not typeahead).
  //
  // The typeahead (PartnerTypeahead.svelte) is a separate component
  // wired into the issue/modification forms; this page's "search"
  // input is for admin browsing, not invoice issuance.

  import { onDestroy, onMount } from "svelte";
  import {
    deletePartner,
    listPartners,
    type Partner,
  } from "../lib/api";
  import {
    EMPTY_PARTNER_FILTER,
    comparePartners,
    filterPartnersWith,
    isPartnerFilterEmpty,
    type PartnerFilterSpec,
    type PartnerKindFacet,
    type PartnerSortKey,
  } from "../lib/partners";
  import type { SortDir } from "../lib/list-sort";
  // PR-68 / session-90 — keyboard navigation Tier-1 UX lift, mirror
  // of the InvoiceList wiring. The existing `.page__search` input
  // becomes the `/`-focus target; j/k walk filtered rows; Enter
  // opens the focused partner's edit modal.
  import {
    makeHotkeyParserState,
    nextRowIndex,
    parseHotkey,
  } from "../lib/keyboard-nav";
  // PR-181 / session-181 — persist the quick-filter needle to
  // localStorage so an operator's typed filter survives reload.
  // PR-194 / session-194 — extended to persist the sort + Kind facet
  // as well, mirroring InvoiceList (S119 / S175). Seeded synchronously
  // at component init (before first render) so the rendered list
  // reflects the persisted state without a flash of the unsorted /
  // unfiltered set.
  import {
    loadPartnerListPrefs,
    savePartnerListPrefs,
  } from "../lib/partner-list-persistence";
  // PR-193 / session-193 — CSV export of the currently-displayed
  // (filtered) partner set. Tier-4 "invisible excellence" lift for
  // the bookkeeper who wants a master-data snapshot in their
  // spreadsheet.
  import {
    composeCsv,
    csvFilenameTimestamp,
    downloadCsv,
  } from "../lib/csv-export";
  import PartnerForm from "./PartnerForm.svelte";

  let rows: Partner[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  // PR-194 / session-194 — seed `filter` + `sort` from `localStorage` so
  // a reload restores the operator's last view. `loadPartnerListPrefs`
  // returns the default blob on every failure path (key absent,
  // malformed JSON, unknown vocab) so the legacy "open filter +
  // natural ordering" posture is the safe fallback.
  const initialPrefs = loadPartnerListPrefs();

  let filter: PartnerFilterSpec = $state({ ...initialPrefs.filter });
  let sort: { key: PartnerSortKey | null; dir: SortDir } = $state({
    ...initialPrefs.sort,
  });

  // Modal state: `null` = closed; `"new"` = create-mode;
  // `Partner` = edit-mode pre-filled from that row.
  let modalState: "new" | Partner | null = $state(null);

  // Inline confirm state for the delete button: holds the partner id
  // pending confirmation, or `null` when no confirm is open.
  let confirmDeleteId: string | null = $state(null);
  let deleteError: string | null = $state(null);

  // PR-194 — filter → sort composition. `sort.key === null` keeps the
  // backend's natural ordering (display_name ASC from `ORDER BY` in
  // `list_partners`); a non-null key sorts client-side via
  // `comparePartners`. The slice() defends Array.prototype.sort's
  // in-place mutation from leaking back into the source `rows` array.
  let filtered = $derived(
    sort.key === null
      ? filterPartnersWith(rows, filter)
      : filterPartnersWith(rows, filter)
          .slice()
          .sort((a, b) => comparePartners(a, b, sort.key as PartnerSortKey, sort.dir)),
  );

  // PR-68 / session-90 — keyboard-nav state. `focusedRowIndex` walks
  // the filtered row set; `hintsVisible` toggles the footer chip.
  // `searchInputEl` is the DOM ref the `/` hotkey focuses.
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

  // PR-181 — persist the needle on every mutation. Cheap (`setItem`
  // on a 30-byte blob); fire-and-forget on failure (private browsing,
  // quota). Runs once at mount with the seeded value too, which is
  // harmless (idempotent write of the same blob the load read).
  // PR-194 / session-194 — extended to persist sort + Kind facet.
  $effect(() => {
    savePartnerListPrefs({
      sort: { key: sort.key, dir: sort.dir },
      filter: { needle: filter.needle, kind: filter.kind },
    });
  });

  onMount(() => {
    void loadPartners();
    window.addEventListener("keydown", handleKeydown);
  });

  onDestroy(() => {
    window.removeEventListener("keydown", handleKeydown);
  });

  function handleKeydown(event: KeyboardEvent) {
    // Stand down while the create/edit modal owns the keyboard.
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
        // PR-194 — same Enter-on-button suppression as InvoiceList:
        // if a <button> (sort header, +New partner, quiet-button) is
        // the focused element, the browser's native handler fires that
        // button's click; emitting row-open on top would double-fire.
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

  async function loadPartners() {
    loadState = "loading";
    loadError = null;
    try {
      rows = await listPartners();
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function openCreate() {
    modalState = "new";
  }

  function openEdit(partner: Partner) {
    modalState = partner;
  }

  function closeModal() {
    modalState = null;
  }

  async function onSaved() {
    // Refresh the list so the just-created or just-updated row appears
    // in the right order (the backend orders by display_name ASC; a
    // client-side merge would diverge from that ordering on rename).
    modalState = null;
    await loadPartners();
  }

  function requestDelete(partnerId: string) {
    confirmDeleteId = partnerId;
    deleteError = null;
  }

  function cancelDelete() {
    confirmDeleteId = null;
    deleteError = null;
  }

  async function confirmDelete(partnerId: string) {
    deleteError = null;
    try {
      await deletePartner(partnerId);
      confirmDeleteId = null;
      await loadPartners();
    } catch (err: unknown) {
      deleteError = err instanceof Error ? err.message : String(err);
    }
  }

  function kindLabel(kind: Partner["kind"]): string {
    return kind;
  }

  // PR-194 / session-194 — three-click sort cycle (mirror of
  // InvoiceList::onSortClick): first click on a column → (column, asc);
  // second click on the same column → desc; third click → reset.
  // Clicking a different column jumps to (clicked, asc) so an
  // inherited dir doesn't surprise the operator.
  function onSortClick(key: PartnerSortKey) {
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

  // PR-194 — `▲` / `▼` glyph next to the active sort column;
  // empty otherwise. Categorical signal is the glyph per ADR-0017
  // §"Adversarial review #4" — colour is not the carrier.
  function sortIndicator(key: PartnerSortKey): string {
    if (sort.key !== key) return "";
    return sort.dir === "asc" ? "▲" : "▼";
  }

  function ariaSortFor(key: PartnerSortKey): "ascending" | "descending" | "none" {
    if (sort.key !== key) return "none";
    return sort.dir === "asc" ? "ascending" : "descending";
  }

  // PR-194 — closed-vocab Kind facet picker values. Kept as a const
  // so the template iterates without hard-coding the list inline.
  const KIND_FACET_OPTIONS: readonly { value: PartnerKindFacet; label: string }[] = [
    { value: "All", label: "All" },
    { value: "Customer", label: "Customer" },
    { value: "Supplier", label: "Supplier" },
    { value: "Both", label: "Both" },
  ];

  // PR-193 / session-193 — compose a single "Street, 1011 Budapest,
  // Hungary"-style address cell from the four nullable address
  // columns. Nulls / empty strings are dropped (no stray ", , ,").
  function composePartnerAddress(partner: Partner): string {
    const cityLine =
      [partner.address_postal_code, partner.address_city]
        .filter((s): s is string => s !== null && s.trim().length > 0)
        .join(" ");
    const parts = [
      partner.address_street,
      cityLine.length > 0 ? cityLine : null,
      partner.address_country,
    ].filter((s): s is string => s !== null && s.trim().length > 0);
    return parts.join(", ");
  }

  // PR-193 / session-193 — CSV export of the currently-filtered
  // partner set. Columns match the screen's master-data shape with
  // the full composed address (the screen collapses to City only) +
  // the VAT-status discriminator the bookkeeper needs to read
  // PrivatePerson rows correctly (`tax_number` is null on those per
  // ADR-0048).
  function exportCsv() {
    const headers = [
      "Display name",
      "Legal name",
      "Kind",
      "VAT status",
      "Tax number",
      "EU VAT",
      "Address",
      "Email",
    ];
    const rowsOut: unknown[][] = filtered.map((p) => [
      p.display_name,
      p.legal_name,
      p.kind,
      p.customer_vat_status,
      p.tax_number ?? "",
      p.eu_vat_number ?? "",
      composePartnerAddress(p),
      p.contact_email ?? "",
    ]);
    const csv = composeCsv(headers, rowsOut);
    const filename = `aberp-partners-${csvFilenameTimestamp()}.csv`;
    downloadCsv(filename, csv);
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Partners</h2>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New partner
      </button>
    </div>
    <p class="page__lede">
      Saved buyers and suppliers. Pick a row in the invoice form's
      typeahead to auto-fill the buyer fields — no retyping the legal
      name, ADÓSZÁM, or address per invoice.
    </p>
  </header>

  <div class="page__toolbar">
    <label class="page__search">
      <span class="visually-hidden">Filter partners</span>
      <input
        bind:this={searchInputEl}
        type="search"
        value={filter.needle}
        oninput={(e) =>
          (filter = {
            ...filter,
            needle: (e.currentTarget as HTMLInputElement).value,
          })}
        placeholder="Filter by name or tax number… (press /)"
        autocomplete="off"
        spellcheck="false"
      />
    </label>
    <!-- PR-194 / session-194 — Kind facet picker. Closed-vocab four
         values (All / Customer / Supplier / Both); ANDs with the
         needle. -->
    <label class="filter">
      <span class="filter-label">Kind</span>
      <select
        value={filter.kind}
        onchange={(e) =>
          (filter = {
            ...filter,
            kind: (e.currentTarget as HTMLSelectElement).value as PartnerKindFacet,
          })}
        aria-label="Filter partners by kind"
      >
        {#each KIND_FACET_OPTIONS as option (option.value)}
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
      title="Export the currently displayed partners to a CSV file"
    >
      Export CSV
    </button>
  </div>

  {#if loadState === "loading"}
    <p class="page__muted">Loading…</p>
  {:else if loadState === "error"}
    <div class="page__error" role="alert">
      <strong>Could not load partners.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else if rows.length === 0}
    <div class="page__empty">
      <p>No partners yet. Add your first.</p>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New partner
      </button>
    </div>
  {:else if filtered.length === 0}
    <p class="page__muted">
      No partner matches the current filter.
      {#if !isPartnerFilterEmpty(filter)}
        <button
          type="button"
          class="quiet-button clear-filters"
          onclick={() => (filter = { ...EMPTY_PARTNER_FILTER })}
        >
          Clear filters
        </button>
      {/if}
    </p>
  {:else}
    <table class="partners-table">
      <thead>
        <tr>
          <!-- PR-194 / session-194 — sortable headers. Each <th>
               renders a button that three-cycles asc → desc → reset
               via `onSortClick`. The ▲/▼ glyph carries the
               categorical signal (ADR-0017 §"Adversarial review #4"). -->
          <th scope="col" aria-sort={ariaSortFor("display_name")}>
            <button
              type="button"
              class="sort-header"
              onclick={() => onSortClick("display_name")}
            >
              <span>Display name</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("display_name")}</span
              >
            </button>
          </th>
          <th scope="col">Legal name</th>
          <th scope="col" aria-sort={ariaSortFor("kind")}>
            <button
              type="button"
              class="sort-header"
              onclick={() => onSortClick("kind")}
            >
              <span>Kind</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("kind")}</span
              >
            </button>
          </th>
          <th scope="col" aria-sort={ariaSortFor("tax_number")}>
            <button
              type="button"
              class="sort-header"
              onclick={() => onSortClick("tax_number")}
            >
              <span>Tax number</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("tax_number")}</span
              >
            </button>
          </th>
          <th scope="col" aria-sort={ariaSortFor("eu_vat")}>
            <button
              type="button"
              class="sort-header"
              onclick={() => onSortClick("eu_vat")}
            >
              <span>EU VAT</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("eu_vat")}</span
              >
            </button>
          </th>
          <th scope="col" aria-sort={ariaSortFor("city")}>
            <button
              type="button"
              class="sort-header"
              onclick={() => onSortClick("city")}
            >
              <span>City</span>
              <span class="sort-indicator" aria-hidden="true"
                >{sortIndicator("city")}</span
              >
            </button>
          </th>
          <th scope="col">Contact</th>
          <th scope="col" class="actions-header">
            <span class="visually-hidden">Actions</span>
          </th>
        </tr>
      </thead>
      <tbody>
        {#each filtered as partner, rowIndex (partner.id)}
          {@const isKeyboardFocused = rowIndex === focusedRowIndex}
          <tr class:row-focused={isKeyboardFocused}>
            <td>{partner.display_name}</td>
            <td>{partner.legal_name}</td>
            <td>
              <span
                class="kind-chip"
                data-kind={partner.kind}
                title={`This partner is treated as ${partner.kind}`}
              >
                {kindLabel(partner.kind)}
              </span>
            </td>
            <td class="mono">{partner.tax_number ?? "—"}</td>
            <td class="mono">{partner.eu_vat_number ?? "—"}</td>
            <td>{partner.address_city ?? "—"}</td>
            <td>{partner.contact_email ?? "—"}</td>
            <td class="actions">
              {#if confirmDeleteId === partner.id}
                <div class="confirm">
                  <span class="confirm__text">
                    Soft-delete <strong>{partner.display_name}</strong>?
                    It stays in the database for historical invoice
                    references.
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
                      onclick={() => void confirmDelete(partner.id)}
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
                  onclick={() => openEdit(partner)}
                >
                  Edit
                </button>
                <button
                  type="button"
                  class="quiet-button"
                  onclick={() => requestDelete(partner.id)}
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

  <!-- PR-68 / session-90 — keyboard hints footer. `?` toggles. -->
  {#if hintsVisible}
    <p class="keyboard-hints" aria-hidden="true">
      Press <kbd>/</kbd> to search • <kbd>j</kbd>/<kbd>k</kbd> to navigate •
      <kbd>Enter</kbd> to edit • <kbd>?</kbd> to hide
    </p>
  {/if}
</section>

{#if modalState !== null}
  <PartnerForm
    partner={modalState === "new" ? null : modalState}
    onSaved={onSaved}
    onClose={closeModal}
  />
{/if}

<style>
  .page {
    max-width: 1200px;
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
  }

  .page__toolbar {
    margin-bottom: var(--space-3);
    /* PR-193 / session-193 — flex so the new "Export CSV" button
     * sits next to the search input with a consistent gap. The
     * pre-PR-193 toolbar held only the search input, so a flex
     * layout was redundant; the second affordance lifts it. */
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
    border-radius: var(--radius-sm);
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
    border-radius: var(--radius-sm);
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

  .partners-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }

  .partners-table th,
  .partners-table td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
    vertical-align: top;
  }

  .partners-table th {
    color: var(--color-text-secondary);
    font-weight: 500;
    background: var(--color-surface-raised);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: var(--type-size-xs);
  }

  /* PR-194 / session-194 — sortable column header buttons. Reset the
   * native button chrome so they read as plain header text; mirror of
   * InvoiceList::.sort-header (ADR-0017 §1-2 quiet chrome; the ▲/▼
   * glyph carries the categorical signal). */
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

  /* PR-194 — Kind facet chip. Mirror of InvoiceList::.filter. */
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

  .filter select:hover {
    border-color: var(--color-text-muted);
  }

  /* PR-194 — "Clear filters" affordance on the empty-state row. */
  .clear-filters {
    margin-left: var(--space-2);
  }

  .partners-table td.mono {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
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
    border-radius: var(--radius-sm);
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quiet-button.danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .kind-chip {
    display: inline-block;
    padding: 0 var(--space-2);
    border-radius: var(--radius-lg);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-size: var(--type-size-xs);
    font-weight: 500;
  }

  .kind-chip[data-kind="Customer"] {
    border-color: var(--color-signal-positive, var(--color-text-strong));
  }

  .kind-chip[data-kind="Supplier"] {
    border-color: var(--color-signal-muted);
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

  /* PR-68 / session-90 — focused-row highlight for the j/k cursor.
   * Mirrors the InvoiceList screen's posture so the two list views
   * share a single keyboard-cursor signal. */
  .partners-table tbody tr.row-focused {
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
    border-radius: var(--radius-sm);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    line-height: 1.4;
  }
</style>
