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
  import { filterPartners } from "../lib/partners";
  // PR-68 / session-90 — keyboard navigation Tier-1 UX lift, mirror
  // of the InvoiceList wiring. The existing `.page__search` input
  // becomes the `/`-focus target; j/k walk filtered rows; Enter
  // opens the focused partner's edit modal.
  import {
    makeHotkeyParserState,
    nextRowIndex,
    parseHotkey,
  } from "../lib/keyboard-nav";
  import PartnerForm from "./PartnerForm.svelte";

  let rows: Partner[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);
  let search: string = $state("");

  // Modal state: `null` = closed; `"new"` = create-mode;
  // `Partner` = edit-mode pre-filled from that row.
  let modalState: "new" | Partner | null = $state(null);

  // Inline confirm state for the delete button: holds the partner id
  // pending confirmation, or `null` when no confirm is open.
  let confirmDeleteId: string | null = $state(null);
  let deleteError: string | null = $state(null);

  let filtered = $derived(filterPartners(rows, search));

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
          if (search.length > 0) {
            search = "";
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
        bind:value={search}
        placeholder="Filter by name or tax number… (press /)"
        autocomplete="off"
        spellcheck="false"
      />
    </label>
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
    <p class="page__muted">No partner matches the current filter.</p>
  {:else}
    <table class="partners-table">
      <thead>
        <tr>
          <th scope="col">Display name</th>
          <th scope="col">Legal name</th>
          <th scope="col">Kind</th>
          <th scope="col">Tax number</th>
          <th scope="col">City</th>
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
            <td class="mono">{partner.tax_number}</td>
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
    border-radius: 4px;
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
    border-radius: 12px;
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
    border-radius: 2px;
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    line-height: 1.4;
  }
</style>
