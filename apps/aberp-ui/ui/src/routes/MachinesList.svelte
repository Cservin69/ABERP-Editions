<script lang="ts">
  // S427 — Machines master-data screen. Mirrors PartnersList.svelte:
  //   1. Open #/machines. Page lists every active machine.
  //   2. Click "+ New machine" or "Edit" → MachineForm modal opens.
  //   3. Click "Archive" on a row → inline confirm → soft-delete; row
  //      disappears (stays in DB; no hard delete).
  //   4. Type in the search box / pick a family → client-side filter.

  import { onMount } from "svelte";
  import {
    archiveMachine,
    listMachines,
    type QuotingMachine,
  } from "../lib/api";
  import {
    EMPTY_MACHINE_FILTER,
    filterMachines,
    isMachineFilterEmpty,
    machineFamilyLabel,
    MACHINE_FAMILIES,
    type MachineFamilyFacet,
    type MachineFilterSpec,
  } from "../lib/machines";
  import MachineForm from "./MachineForm.svelte";

  let rows: QuotingMachine[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  let filter: MachineFilterSpec = $state({ ...EMPTY_MACHINE_FILTER });

  // Modal state: `null` = closed; `"new"` = create-mode;
  // `QuotingMachine` = edit-mode pre-filled from that row.
  let modalState: "new" | QuotingMachine | null = $state(null);

  // Inline confirm state for the archive button.
  let confirmArchiveId: string | null = $state(null);
  let archiveError: string | null = $state(null);

  let filtered = $derived(filterMachines(rows, filter));

  // Family facet picker values (All + the eight closed-vocab families).
  const FAMILY_FACET_OPTIONS: readonly { value: MachineFamilyFacet; label: string }[] = [
    { value: "All", label: "All" },
    ...MACHINE_FAMILIES,
  ];

  onMount(() => {
    void loadMachines();
  });

  async function loadMachines() {
    loadState = "loading";
    loadError = null;
    try {
      rows = await listMachines();
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function openCreate() {
    modalState = "new";
  }

  function openEdit(machine: QuotingMachine) {
    modalState = machine;
  }

  function closeModal() {
    modalState = null;
  }

  async function onSaved() {
    modalState = null;
    await loadMachines();
  }

  function requestArchive(machineId: string) {
    confirmArchiveId = machineId;
    archiveError = null;
  }

  function cancelArchive() {
    confirmArchiveId = null;
    archiveError = null;
  }

  async function confirmArchive(machineId: string) {
    archiveError = null;
    try {
      await archiveMachine(machineId);
      confirmArchiveId = null;
      await loadMachines();
    } catch (err: unknown) {
      archiveError = err instanceof Error ? err.message : String(err);
    }
  }

  function envelopeCell(machine: QuotingMachine): string {
    return machine.max_envelope_xyz_mm.map((n) => String(n)).join(" × ");
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Machines</h2>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New machine
      </button>
    </div>
    <p class="page__lede">
      Your machine park — family, working envelope and daily capacity.
      The auto-quoting engine reads this to size and schedule each part.
    </p>
  </header>

  <div class="page__toolbar">
    <label class="page__search">
      <span class="visually-hidden">Filter machines</span>
      <input
        type="search"
        value={filter.needle}
        oninput={(e) =>
          (filter = {
            ...filter,
            needle: (e.currentTarget as HTMLInputElement).value,
          })}
        placeholder="Filter by name…"
        autocomplete="off"
        spellcheck="false"
      />
    </label>
    <label class="filter">
      <span class="filter-label">Family</span>
      <select
        value={filter.family}
        onchange={(e) =>
          (filter = {
            ...filter,
            family: (e.currentTarget as HTMLSelectElement)
              .value as MachineFamilyFacet,
          })}
        aria-label="Filter machines by family"
      >
        {#each FAMILY_FACET_OPTIONS as option (option.value)}
          <option value={option.value}>{option.label}</option>
        {/each}
      </select>
    </label>
  </div>

  {#if loadState === "loading"}
    <p class="page__muted">Loading…</p>
  {:else if loadState === "error"}
    <div class="page__error" role="alert">
      <strong>Could not load machines.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else if rows.length === 0}
    <div class="page__empty">
      <p>No machines yet. Add your first.</p>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New machine
      </button>
    </div>
  {:else if filtered.length === 0}
    <p class="page__muted">
      No machine matches the current filter.
      {#if !isMachineFilterEmpty(filter)}
        <button
          type="button"
          class="quiet-button clear-filters"
          onclick={() => (filter = { ...EMPTY_MACHINE_FILTER })}
        >
          Clear filters
        </button>
      {/if}
    </p>
  {:else}
    <table class="machines-table">
      <thead>
        <tr>
          <th scope="col">Name</th>
          <th scope="col">Family</th>
          <th scope="col">Envelope (mm)</th>
          <th scope="col">Daily hours</th>
          <th scope="col">Buffer %</th>
          <th scope="col">Enabled</th>
          <th scope="col" class="actions-header">
            <span class="visually-hidden">Actions</span>
          </th>
        </tr>
      </thead>
      <tbody>
        {#each filtered as machine (machine.id)}
          <tr>
            <td>{machine.name}</td>
            <td>
              <span class="family-chip">{machineFamilyLabel(machine.family)}</span>
            </td>
            <td class="mono">{envelopeCell(machine)}</td>
            <td class="mono">{machine.daily_hours_avail}</td>
            <td class="mono">{machine.buffer_pct}</td>
            <td>
              {#if machine.enabled}
                <span class="enabled-chip">Yes</span>
              {:else}
                <span class="disabled-chip">No</span>
              {/if}
            </td>
            <td class="actions">
              {#if confirmArchiveId === machine.id}
                <div class="confirm">
                  <span class="confirm__text">
                    Archive <strong>{machine.name}</strong>? It stays in
                    the database for historical references.
                  </span>
                  <div class="confirm__buttons">
                    <button
                      type="button"
                      class="quiet-button"
                      onclick={cancelArchive}
                    >
                      Cancel
                    </button>
                    <button
                      type="button"
                      class="quiet-button danger"
                      onclick={() => void confirmArchive(machine.id)}
                    >
                      Archive
                    </button>
                  </div>
                  {#if archiveError !== null}
                    <p class="confirm__error" role="alert">{archiveError}</p>
                  {/if}
                </div>
              {:else}
                <button
                  type="button"
                  class="quiet-button"
                  onclick={() => openEdit(machine)}
                >
                  Edit
                </button>
                <button
                  type="button"
                  class="quiet-button"
                  onclick={() => requestArchive(machine.id)}
                >
                  Archive
                </button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if modalState !== null}
  <MachineForm
    machine={modalState === "new" ? null : modalState}
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

  .machines-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }

  .machines-table th,
  .machines-table td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
    vertical-align: top;
  }

  .machines-table th {
    color: var(--color-text-secondary);
    font-weight: 500;
    background: var(--color-surface-raised);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: var(--type-size-xs);
  }

  .machines-table td.mono {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

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

  .clear-filters {
    margin-left: var(--space-2);
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

  .family-chip {
    display: inline-block;
    padding: 0 var(--space-2);
    border-radius: 12px;
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-size: var(--type-size-xs);
    font-weight: 500;
  }

  .enabled-chip,
  .disabled-chip {
    display: inline-block;
    padding: 0 var(--space-2);
    border-radius: 12px;
    border: 1px solid var(--color-surface-divider);
    font-size: var(--type-size-xs);
    font-weight: 500;
  }

  .enabled-chip {
    color: var(--color-signal-positive, var(--color-text-strong));
    border-color: var(--color-signal-positive, var(--color-text-strong));
  }

  .disabled-chip {
    color: var(--color-text-muted);
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
</style>
