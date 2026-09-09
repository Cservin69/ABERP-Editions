<script lang="ts">
  // ADR-0112 Part C (D-19 slice C3) — Drilling Rates screen
  // (Maintenance → Quoting). Defense-only.
  //   1. Open #/quoting-drilling-rates. Page lists every material-group rate.
  //   2. "+ New rate" / "Edit" → DrillingRateForm modal.
  //   3. "Delete" on a row → inline confirm → hard delete (that material then
  //      prices no drilling until a row is re-added).
  // Seeded INERT (feed 0) per material group on a fresh Defense tenant: the
  // engine prices no drilling until the operator sets a real cutting feed.
  // On a Portable build the backend answers 403 and this screen shows a
  // Defense-only notice instead of an editable table.

  import { onMount } from "svelte";
  import { deleteDrillingRate, listDrillingRates, type DrillingRate } from "../lib/api";
  import { drillingRateStatusLabel, isInertDrillingRate } from "../lib/drilling-rates";
  import DrillingRateForm from "./DrillingRateForm.svelte";

  let rows: DrillingRate[] = $state([]);
  let loadState: "loading" | "loaded" | "error" | "unavailable" = $state("loading");
  let loadError: string | null = $state(null);

  // `null` = closed; `"new"` = create-mode; a row = edit-mode.
  let modalState: "new" | DrillingRate | null = $state(null);

  let confirmDeleteId: string | null = $state(null);
  let deleteError: string | null = $state(null);

  onMount(() => {
    void loadRates();
  });

  function isEditionRefusal(message: string): boolean {
    // The serve.rs guard answers 403 with a body naming the boundary; the
    // Tauri bridge surfaces that text. Recognise it so a Portable build shows
    // a calm "Defense-only" notice rather than a red error.
    return message.includes("Defense-only capability");
  }

  async function loadRates() {
    loadState = "loading";
    loadError = null;
    try {
      const res = await listDrillingRates();
      rows = res.rates;
      loadState = "loaded";
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      if (isEditionRefusal(message)) {
        loadState = "unavailable";
      } else {
        loadState = "error";
        loadError = message;
      }
    }
  }

  function openCreate() {
    modalState = "new";
  }

  function openEdit(rate: DrillingRate) {
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
      await deleteDrillingRate(id);
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
      <h2 id="page-title" class="page__title">Drilling rates</h2>
      {#if loadState === "loaded"}
        <button type="button" class="page__primary" onclick={openCreate}>
          + New rate
        </button>
      {/if}
    </div>
    <p class="page__lede">
      Per-material drilling cycle-time coefficients (cutting feed, peck policy,
      rapids, tool-change time and end-condition penalties). The auto-quoting
      engine prices each located hole from these when a part carries drilled
      geometry (ADR-0112 Part&nbsp;C). A row with feed&nbsp;0 is
      <strong>inert</strong> — no drilling is priced for that material until you
      set a real feed measured on your machines.
    </p>
  </header>

  {#if loadState === "loading"}
    <p class="state">Loading…</p>
  {:else if loadState === "unavailable"}
    <p class="state state--muted">
      The drilling cost model is a Defense-only capability and is compiled out
      of this edition. The local quote engine and manual quoting remain
      available.
    </p>
  {:else if loadState === "error"}
    <p class="state state--error">Could not load rates: {loadError}</p>
  {:else if rows.length === 0}
    <p class="state">
      No drilling rates yet. Add one per material group to price drilled
      geometry.
    </p>
  {:else}
    <table class="grid">
      <thead>
        <tr>
          <th>Material group</th>
          <th class="num">Feed (mm/min·⌀)</th>
          <th class="num">Peck ×⌀</th>
          <th class="num">Tool change (s)</th>
          <th>Status</th>
          <th class="actions-col">Actions</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as rate (rate.id)}
          <tr class:inert={isInertDrillingRate(rate)}>
            <td>{rate.material_group}</td>
            <td class="num">{rate.feed_mm_per_min_per_mm_dia}</td>
            <td class="num">{rate.peck_depth_dia_multiple}</td>
            <td class="num">{rate.tool_change_sec}</td>
            <td class="muted">{drillingRateStatusLabel(rate)}</td>
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
  <DrillingRateForm
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

  tr.inert td {
    color: var(--color-text-muted);
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

  .state--muted {
    color: var(--color-text-muted);
    max-width: 64ch;
  }

  .state--error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
  }
</style>
