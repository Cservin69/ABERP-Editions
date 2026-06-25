<script lang="ts">
  // S4 / ADR-0094 Gap 2 — Machine Rates screen (Maintenance → Quoting).
  //   1. Open #/quoting-machine-rates. Page lists every family rate.
  //   2. "+ New rate" / "Edit" → MachineRateForm modal.
  //   3. "Delete" on a row → inline confirm → hard delete (the family then
  //      falls back to the engine's global flat rate; no orphaned pricing).
  // Seeded with the six ADR-0094 families on a fresh tenant. Mirrors
  // MachinesList.svelte (family-keyed CRUD, dark CSS tokens).

  import { onMount } from "svelte";
  import { deleteMachineRate, listMachineRates, type MachineRate } from "../lib/api";
  import {
    effectiveLightsOutLabel,
    machineFamilyLabel,
  } from "../lib/machine-rates";
  import MachineRateForm from "./MachineRateForm.svelte";

  let rows: MachineRate[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  // `null` = closed; `"new"` = create-mode; a row = edit-mode.
  let modalState: "new" | MachineRate | null = $state(null);

  let confirmDeleteId: string | null = $state(null);
  let deleteError: string | null = $state(null);

  onMount(() => {
    void loadRates();
  });

  async function loadRates() {
    loadState = "loading";
    loadError = null;
    try {
      const res = await listMachineRates();
      rows = res.rates;
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function openCreate() {
    modalState = "new";
  }

  function openEdit(rate: MachineRate) {
    modalState = rate;
  }

  function closeModal() {
    modalState = null;
  }

  async function onSaved() {
    modalState = null;
    await loadRates();
  }

  function requestDelete(id: string) {
    confirmDeleteId = id;
    deleteError = null;
  }

  function cancelDelete() {
    confirmDeleteId = null;
    deleteError = null;
  }

  async function confirmDelete(id: string) {
    deleteError = null;
    try {
      await deleteMachineRate(id);
      confirmDeleteId = null;
      await loadRates();
    } catch (err: unknown) {
      deleteError = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Machine rates</h2>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New rate
      </button>
    </div>
    <p class="page__lede">
      Per-family machine cost (EUR/min) and the lights-out (unattended)
      discount factor. The auto-quoting engine routes each part to a
      family and charges its effective rate — a bar-fed Swiss running
      lights-out prices a small turned part below an attended mill
      (ADR-0094 Gap&nbsp;2). A family with no row falls back to the
      global flat rate.
    </p>
  </header>

  {#if loadState === "loading"}
    <p class="state">Loading…</p>
  {:else if loadState === "error"}
    <p class="state state--error">Could not load rates: {loadError}</p>
  {:else if rows.length === 0}
    <p class="state">No machine rates yet. Add one to override the global rate for a family.</p>
  {:else}
    <table class="grid">
      <thead>
        <tr>
          <th>Family</th>
          <th class="num">Attended €/min</th>
          <th class="num">Lights-out ×</th>
          <th>Unattended</th>
          <th>Effective</th>
          <th class="actions-col">Actions</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as rate (rate.id)}
          <tr>
            <td>{machineFamilyLabel(rate.family)}</td>
            <td class="num">{rate.attended_rate_eur_per_min.toFixed(4)}</td>
            <td class="num">{rate.lights_out_factor.toFixed(4)}</td>
            <td>{rate.unattended_capable ? "yes" : "no"}</td>
            <td class="muted">{effectiveLightsOutLabel(rate)}</td>
            <td class="actions-col">
              {#if confirmDeleteId === rate.id}
                <span class="confirm">
                  Delete?
                  <button type="button" class="link link--danger" onclick={() => confirmDelete(rate.id)}>
                    Yes
                  </button>
                  <button type="button" class="link" onclick={cancelDelete}>No</button>
                </span>
              {:else}
                <button type="button" class="link" onclick={() => openEdit(rate)}>Edit</button>
                <button type="button" class="link link--danger" onclick={() => requestDelete(rate.id)}>
                  Delete
                </button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
    {#if deleteError !== null}
      <p class="state state--error">Delete failed: {deleteError}</p>
    {/if}
  {/if}
</section>

{#if modalState !== null}
  <MachineRateForm
    rate={modalState === "new" ? null : modalState}
    {onSaved}
    onClose={closeModal}
  />
{/if}

<style>
  .page {
    padding: var(--space-4) var(--space-5);
    color: var(--color-text-primary);
  }

  .page__head-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
  }

  .page__title {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .page__lede {
    margin: var(--space-2) 0 var(--space-4) 0;
    max-width: 64ch;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
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

  .grid {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }

  .grid th,
  .grid td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
  }

  .grid th {
    color: var(--color-text-muted);
    font-weight: 600;
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .grid td.num,
  .grid th.num {
    text-align: right;
    font-family: var(--type-family-mono);
  }

  .muted {
    color: var(--color-text-muted);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
  }

  .actions-col {
    text-align: right;
    white-space: nowrap;
  }

  .link {
    background: none;
    border: 0;
    color: var(--color-text-secondary);
    cursor: pointer;
    font-size: var(--type-size-sm);
    padding: 0 var(--space-2);
  }

  .link:hover {
    color: var(--color-text-strong);
  }

  .link--danger {
    color: var(--color-signal-negative);
  }

  .confirm {
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }

  .state {
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }

  .state--error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
  }
</style>
