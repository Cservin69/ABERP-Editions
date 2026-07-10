<script lang="ts">
  // S6 / ADR-0094 Gap 3 — Gear Processes screen (Maintenance → Quoting).
  //   1. Open #/quoting-gear-processes. Page lists every process coefficient.
  //   2. "+ New process" / "Edit" → GearProcessForm modal.
  //   3. "Delete" on a row → inline confirm → hard delete (a gear that still
  //      resolves to a deleted process then contributes 0.0 + a loud engine
  //      reasoning line — fail-soft per the S5 handoff).
  // Seeded with the five concrete ADR-0094 processes on a fresh tenant.
  // Mirrors MachineRatesList.svelte (keyed CRUD, dark CSS tokens). Inert: the
  // table only bites once a part carries gear ops.

  import { onMount } from "svelte";
  import {
    deleteGearProcess,
    listGearProcesses,
    type GearProcessRate,
  } from "../lib/api";
  import { gearProcessLabel } from "../lib/gear-processes";
  import GearProcessForm from "./GearProcessForm.svelte";

  let rows: GearProcessRate[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  // `null` = closed; `"new"` = create-mode; a row = edit-mode.
  let modalState: "new" | GearProcessRate | null = $state(null);

  let confirmDeleteId: string | null = $state(null);
  let deleteError: string | null = $state(null);

  onMount(() => {
    void loadProcesses();
  });

  async function loadProcesses() {
    loadState = "loading";
    loadError = null;
    try {
      const res = await listGearProcesses();
      rows = res.processes;
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function openCreate() {
    modalState = "new";
  }

  function openEdit(rate: GearProcessRate) {
    modalState = rate;
  }

  function closeModal() {
    modalState = null;
  }

  async function onSaved() {
    modalState = null;
    await loadProcesses();
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
      await deleteGearProcess(id);
      confirmDeleteId = null;
      await loadProcesses();
    } catch (err: unknown) {
      deleteError = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Gear processes</h2>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New process
      </button>
    </div>
    <p class="page__lede">
      Per-process tooth-generation time coefficients (setup, minutes/tooth,
      module exponent, AGMA quality factor, in-cycle factor). The auto-quoting
      engine costs a part's gear ops at the routed family's effective €/min —
      external hob/skive cheap (power-skiving runs in-cycle on a turn-mill),
      internal shape/broach/wire-EDM premium (ADR-0094 Gap&nbsp;3). A part with
      no gear ops carries no gear cost (inert); a gear whose process has no row
      contributes €0 + a loud reasoning line.
    </p>
  </header>

  {#if loadState === "loading"}
    <p class="state">Loading…</p>
  {:else if loadState === "error"}
    <p class="state state--error">Could not load processes: {loadError}</p>
  {:else if rows.length === 0}
    <p class="state">
      No gear processes yet. Add one so geared parts carry an itemised cost.
    </p>
  {:else}
    <table class="grid">
      <thead>
        <tr>
          <th>Process</th>
          <th class="num">Setup (min)</th>
          <th class="num">Per tooth</th>
          <th class="num">Module exp</th>
          <th class="num">AGMA base</th>
          <th class="num">In-cycle ×</th>
          <th class="actions-col">Actions</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as proc (proc.id)}
          <tr>
            <td>{gearProcessLabel(proc.process)}</td>
            <td class="num">{proc.setup_min.toFixed(2)}</td>
            <td class="num">{proc.min_per_tooth.toFixed(4)}</td>
            <td class="num">{proc.module_exponent.toFixed(2)}</td>
            <td class="num">{proc.agma_quality_factor_base.toFixed(4)}</td>
            <td class="num">{proc.in_cycle_factor.toFixed(2)}</td>
            <td class="actions-col">
              {#if confirmDeleteId === proc.id}
                <span class="confirm">
                  Delete?
                  <button type="button" class="link link--danger" onclick={() => confirmDelete(proc.id)}>
                    Yes
                  </button>
                  <button type="button" class="link" onclick={cancelDelete}>No</button>
                </span>
              {:else}
                <button type="button" class="link" onclick={() => openEdit(proc)}>Edit</button>
                <button type="button" class="link link--danger" onclick={() => requestDelete(proc.id)}>
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
  <GearProcessForm
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
    border-radius: var(--radius-sm);
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
