<script lang="ts">
  // T5 / ADR-0097 Part 2 — Tolerance cost-rates screen (Maintenance → Quoting).
  //   1. Open #/quoting-tolerance-cost-rates. Page lists the per-band rows.
  //   2. "+ New rate" / "Edit" → QuotingToleranceCostRateForm modal.
  //   3. "Delete" on a row → inline confirm → hard delete (the band then
  //      contributes a zero tolerance_cost in the engine; no orphaned pricing).
  // Seeded zero-contribution (one row per band) on a fresh tenant, so the
  // catalogue has rows to tune but moves no money until the operator does
  // (ADR-0097 R4). Mirrors MachineRatesList.svelte (dark CSS tokens).

  import { onMount } from "svelte";
  import {
    deleteToleranceCostRate,
    listToleranceCostRates,
    type ToleranceCostRate,
  } from "../lib/api";
  import { isZeroContribution } from "../lib/tolerance-cost-rates";
  import { fmtPct, toleranceRangeLabel } from "../lib/quoting-tunables-format";
  import QuotingToleranceCostRateForm from "./QuotingToleranceCostRateForm.svelte";

  let rows: ToleranceCostRate[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  // `null` = closed; `"new"` = create-mode; a row = edit-mode.
  let modalState: "new" | ToleranceCostRate | null = $state(null);

  let confirmDeleteId: string | null = $state(null);
  let deleteError: string | null = $state(null);

  onMount(() => {
    void loadRates();
  });

  async function loadRates() {
    loadState = "loading";
    loadError = null;
    try {
      const res = await listToleranceCostRates();
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

  function openEdit(rate: ToleranceCostRate) {
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
      await deleteToleranceCostRate(id);
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
      <h2 id="page-title" class="page__title">Tolerance cost rates</h2>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New rate
      </button>
    </div>
    <p class="page__lede">
      Per-band machining-tolerance cost drivers. The auto-quoting engine adds an
      itemised <code>tolerance_cost</code> line — in-process gauging, CMM time
      per critical feature, scrap/rework uplift, slower-feed finishing passes
      and (at the tightest band) a grinding adder — at the routed effective
      €/min (ADR-0097). Seeded zero-contribution per band, so nothing moves
      until you tune a value; a band with no row contributes nothing.
    </p>
  </header>

  {#if loadState === "loading"}
    <p class="state">Loading…</p>
  {:else if loadState === "error"}
    <p class="state state--error">Could not load rates: {loadError}</p>
  {:else if rows.length === 0}
    <p class="state">No tolerance cost rates yet. Add one to price a band's drivers.</p>
  {:else}
    <table class="grid">
      <thead>
        <tr>
          <th>Band</th>
          <th class="num">Finish +</th>
          <th class="num">In-proc min</th>
          <th class="num">CMM min/feat</th>
          <th class="num">Scrap/rework</th>
          <th class="num">Feed ×</th>
          <th>Grinding</th>
          <th>Status</th>
          <th class="actions-col">Actions</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as rate (rate.id)}
          <tr>
            <td>{toleranceRangeLabel(rate.tolerance_class)}</td>
            <td class="num">{rate.finish_passes_add.toFixed(2)}</td>
            <td class="num">{rate.inproc_inspection_min.toFixed(2)}</td>
            <td class="num">{rate.cmm_min_per_critical_feature.toFixed(2)}</td>
            <td class="num">{fmtPct(rate.rework_scrap_pct)}</td>
            <td class="num">{rate.feed_slowdown_factor.toFixed(4)}</td>
            <td>{rate.grinding_escalation ? "yes" : "no"}</td>
            <td class="muted">{isZeroContribution(rate) ? "dormant (0)" : "tuned"}</td>
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
  <QuotingToleranceCostRateForm
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
