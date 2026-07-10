<script lang="ts">
  // S257 / PR-246 — Settings → Adapters page. Operator-managed MES
  // adapter lifecycle: list (config joined with live registry health),
  // add (starts immediately), edit (hot restart in place), delete
  // (stop + deregister). Replaces the env-var + restart workflow per
  // [[trust-code-not-operator]] + [[hulye-biztos]].
  //
  // Dark-theme per [[spa-dark-theme-default]] — modelled on
  // QuotesList.svelte's token vocab; ConfirmActionModal for delete.
  //
  // Demo mode ([[aberp-workshop-demo-mode]]): the page still renders,
  // but every mutation is refused with a clear banner so a demo-tour
  // operator can't muck with real adapter config.

  import { onMount } from "svelte";
  import {
    deleteAdapter,
    listAdapters,
    type AdapterListItem,
  } from "../lib/api";
  import {
    adapterKindLabel,
    adapterStatusLabel,
    adapterStatusTone,
  } from "../lib/adapter-format";
  import { isDemoMode } from "../lib/workshop-demo-mode";
  import ConfirmActionModal from "../lib/ConfirmActionModal.svelte";
  import AdapterForm from "./AdapterForm.svelte";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let rows = $state<AdapterListItem[]>([]);

  // Form (add/edit) modal state.
  let formMode = $state<"add" | "edit" | null>(null);
  let formInitial = $state<AdapterListItem | null>(null);

  // Delete-confirm modal state.
  let confirmDelete = $state<AdapterListItem | null>(null);
  let deleting = $state(false);
  let deleteError = $state<string | null>(null);

  const demo = isDemoMode();

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      rows = await listAdapters();
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function openAdd(): void {
    if (demo) return;
    formInitial = null;
    formMode = "add";
  }

  function openEdit(row: AdapterListItem): void {
    if (demo) return;
    formInitial = row;
    formMode = "edit";
  }

  function closeForm(): void {
    formMode = null;
    formInitial = null;
  }

  async function onFormSaved(): Promise<void> {
    closeForm();
    await refresh();
  }

  function askDelete(row: AdapterListItem): void {
    if (demo) return;
    deleteError = null;
    confirmDelete = row;
  }

  async function doDelete(): Promise<void> {
    if (confirmDelete === null) return;
    deleting = true;
    deleteError = null;
    try {
      await deleteAdapter(confirmDelete.adapter_id);
      confirmDelete = null;
      await refresh();
    } catch (e) {
      deleteError = e instanceof Error ? e.message : String(e);
    } finally {
      deleting = false;
    }
  }
</script>

<section class="adapters-page" data-testid="adapters-list-section">
  <header class="adapters-page__head">
    <div>
      <h2 class="adapters-page__title">
        Adapterek / Adapters
        <span class="adapters-page__hint">
          Gyártási adapterek — hozzáadás, szerkesztés, törlés újraindítás
          nélkül
        </span>
      </h2>
    </div>
    <div class="adapters-page__actions">
      <button
        type="button"
        class="adapters-page__refresh"
        disabled={loadState === "loading"}
        onclick={() => void refresh()}
        data-testid="adapters-refresh"
      >
        {loadState === "loading" ? "Frissítés…" : "Frissítés / Refresh"}
      </button>
      <button
        type="button"
        class="adapters-page__add"
        disabled={demo}
        onclick={openAdd}
        data-testid="adapters-add-btn"
        title={demo ? "Demo mode — adapter changes disabled" : "Add adapter"}
      >
        + Adapter
      </button>
    </div>
  </header>

  {#if demo}
    <div class="adapters-page__demo" role="status" data-testid="adapters-demo-banner">
      Demo mód — az adapterek módosítása letiltva. / Demo mode — adapter
      changes disabled.
    </div>
  {/if}

  {#if deleteError !== null}
    <div class="adapters-page__error" role="alert" data-testid="adapters-delete-error">
      <strong>A törlés nem sikerült / Delete failed.</strong>
      <p class="adapters-page__error-detail">{deleteError}</p>
    </div>
  {/if}

  {#if loadState === "loading" && rows.length === 0}
    <p class="adapters-page__muted">Betöltés… / Loading adapters…</p>
  {:else if loadState === "error"}
    <div class="adapters-page__error" role="alert">
      <strong>Nem sikerült lekérni az adaptereket / Failed to load adapters.</strong>
      <p class="adapters-page__error-detail">{errorMessage}</p>
    </div>
  {:else if rows.length === 0}
    <div class="adapters-page__empty" data-testid="adapters-empty">
      <p>
        Nincs még konfigurált adapter. Adj hozzá egyet a „+ Adapter"
        gombbal — azonnal elindul, újraindítás nélkül.
      </p>
      <p>
        No adapters configured yet. Add one with "+ Adapter" — it starts
        immediately, no restart needed.
      </p>
    </div>
  {:else}
    <table class="adapters-table" data-testid="adapters-table">
      <thead>
        <tr>
          <th scope="col">Megnevezés / Name</th>
          <th scope="col">Típus / Kind</th>
          <th scope="col">Végpont / Endpoint</th>
          <th scope="col">Állapot / State</th>
          <th scope="col">Művelet / Action</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row (row.adapter_id)}
          <tr data-testid="adapters-row" data-adapter-id={row.adapter_id}>
            <td>
              <div class="adapters-table__name">{row.friendly_name}</div>
              <code class="adapters-table__id" title={row.adapter_id}>
                {row.adapter_id}
              </code>
            </td>
            <td>{adapterKindLabel(row.kind)}</td>
            <td class="adapters-table__endpoint">{row.host}:{row.port}</td>
            <td>
              <span
                class="adapters-chip adapters-chip--{adapterStatusTone(row.status)}"
                data-testid="adapters-status-chip"
                data-status={row.status}
              >
                {adapterStatusLabel(row.status)}
              </span>
            </td>
            <td>
              <div class="adapters-row__actions">
                <button
                  type="button"
                  class="adapters-row__edit"
                  disabled={demo}
                  onclick={() => openEdit(row)}
                  data-testid="adapters-row-edit-btn"
                  title={demo
                    ? "Demo mode — adapter changes disabled"
                    : "Edit adapter"}
                >
                  Szerkesztés / Edit
                </button>
                <button
                  type="button"
                  class="adapters-row__delete"
                  disabled={demo}
                  onclick={() => askDelete(row)}
                  data-testid="adapters-row-delete-btn"
                  title={demo
                    ? "Demo mode — adapter changes disabled"
                    : "Delete adapter"}
                >
                  Törlés / Delete
                </button>
              </div>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if formMode !== null}
  <AdapterForm
    mode={formMode}
    initial={formInitial}
    onSaved={() => void onFormSaved()}
    onCancel={closeForm}
  />
{/if}

{#if confirmDelete !== null}
  <ConfirmActionModal
    title="Adapter törlése? / Delete adapter?"
    body={`A(z) "${confirmDelete.friendly_name}" adapter leáll és törlődik. / The "${confirmDelete.friendly_name}" adapter will be stopped and removed.`}
    consequence="Az adapter azonnal leáll. A konfiguráció törlődik a seller.toml-ból. / The adapter stops immediately and its config is removed from seller.toml."
    confirmLabel="Törlés / Delete"
    cancelLabel="Mégse / Cancel"
    busy={deleting}
    onConfirm={() => void doDelete()}
    onCancel={() => (confirmDelete = null)}
  />
{/if}

<style>
  .adapters-page {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) 0;
  }

  .adapters-page__head {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: var(--space-3);
    flex-wrap: wrap;
  }

  .adapters-page__title {
    font-size: var(--type-size-lg);
    font-weight: 600;
    margin: 0;
    color: var(--color-text-strong);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .adapters-page__hint {
    font-size: var(--type-size-sm);
    font-weight: 400;
    color: var(--color-text-muted);
  }

  .adapters-page__actions {
    display: flex;
    gap: var(--space-2);
  }

  .adapters-page__refresh,
  .adapters-page__add {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .adapters-page__add {
    color: var(--color-text-strong);
    border-color: var(--color-signal-positive);
  }

  .adapters-page__refresh:hover:not(:disabled),
  .adapters-page__add:hover:not(:disabled) {
    color: var(--color-signal-positive);
  }

  .adapters-page__refresh:disabled,
  .adapters-page__add:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .adapters-page__demo {
    padding: var(--space-2) var(--space-3);
    border: 1px dashed var(--color-signal-warning);
    background: var(--color-surface-raised);
    color: var(--color-signal-warning);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
  }

  .adapters-page__muted {
    color: var(--color-text-muted);
    font-style: italic;
  }

  .adapters-page__error {
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-negative);
    border-radius: var(--radius-sm);
    color: var(--color-text-primary);
  }

  .adapters-page__error strong {
    color: var(--color-signal-negative);
  }

  .adapters-page__error-detail {
    margin-top: var(--space-1);
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    color: var(--color-text-muted);
  }

  .adapters-page__empty {
    padding: var(--space-4);
    background: var(--color-surface-raised);
    border: 1px dashed var(--color-surface-divider);
    border-radius: var(--radius-sm);
    color: var(--color-text-secondary);
  }

  .adapters-page__empty p + p {
    margin-top: var(--space-2);
  }

  .adapters-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
    background: var(--color-surface-sunken);
  }

  .adapters-table th,
  .adapters-table td {
    padding: var(--space-2) var(--space-3);
    text-align: left;
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
  }

  .adapters-table td {
    color: var(--color-text-primary);
  }

  .adapters-table th {
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .adapters-table tbody tr:hover {
    background: var(--color-surface-raised);
  }

  .adapters-table__name {
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .adapters-table__id {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .adapters-table__endpoint {
    font-family: var(--type-family-mono);
    color: var(--color-text-secondary);
  }

  .adapters-chip {
    display: inline-block;
    padding: 2px 8px;
    border-radius: var(--radius-pill);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    font-weight: 500;
    white-space: nowrap;
  }

  .adapters-chip--positive {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  .adapters-chip--warning {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  .adapters-chip--negative {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .adapters-chip--muted {
    color: var(--color-text-muted);
  }

  .adapters-row__actions {
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
  }

  .adapters-row__edit,
  .adapters-row__delete {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .adapters-row__edit:hover:not(:disabled) {
    color: var(--color-text-strong);
    border-color: var(--color-text-strong);
  }

  .adapters-row__delete:hover:not(:disabled) {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .adapters-row__edit:disabled,
  .adapters-row__delete:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
