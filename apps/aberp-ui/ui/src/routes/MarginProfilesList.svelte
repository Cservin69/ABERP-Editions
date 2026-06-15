<script lang="ts">
  // S428 — Margin Profiles master-data screen. Mirrors MachinesList:
  //   1. Open #/margin-profiles. Page lists every active profile.
  //   2. Click "+ New profile" or "Edit" → MarginProfileForm modal.
  //   3. Click "Archive" → inline confirm → soft-delete (stays in DB).
  //   4. Type in the search box → client-side filter.

  import { onMount } from "svelte";
  import {
    archiveMarginProfile,
    listMarginProfiles,
    type MarginProfile,
  } from "../lib/api";
  import { customerTypeLabel } from "../lib/partners";
  import { filterProfiles, formatPercent } from "../lib/margin-profiles";
  import MarginProfileForm from "./MarginProfileForm.svelte";

  let rows: MarginProfile[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  let needle = $state("");

  // Modal state: `null` = closed; `"new"` = create; row = edit-mode.
  let modalState: "new" | MarginProfile | null = $state(null);

  let confirmArchiveId: string | null = $state(null);
  let archiveError: string | null = $state(null);

  let filtered = $derived(filterProfiles(rows, needle));

  onMount(() => {
    void load();
  });

  async function load() {
    loadState = "loading";
    loadError = null;
    try {
      rows = await listMarginProfiles();
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function openCreate() {
    modalState = "new";
  }

  function openEdit(profile: MarginProfile) {
    modalState = profile;
  }

  function closeModal() {
    modalState = null;
  }

  async function onSaved() {
    modalState = null;
    await load();
  }

  function requestArchive(id: string) {
    confirmArchiveId = id;
    archiveError = null;
  }

  function cancelArchive() {
    confirmArchiveId = null;
    archiveError = null;
  }

  async function confirmArchive(id: string) {
    archiveError = null;
    try {
      await archiveMarginProfile(id);
      confirmArchiveId = null;
      await load();
    } catch (err: unknown) {
      archiveError = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Margin profiles</h2>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New profile
      </button>
    </div>
    <p class="page__lede">
      Per customer-type margin policy. When a quote's buyer has a matching
      customer type, the auto-quoting engine applies this profile's target
      margin and refuses a DEAL below its floor.
    </p>
  </header>

  <div class="page__toolbar">
    <label class="page__search">
      <span class="visually-hidden">Filter profiles</span>
      <input
        type="search"
        bind:value={needle}
        placeholder="Filter by name or customer type…"
        autocomplete="off"
        spellcheck="false"
      />
    </label>
  </div>

  {#if loadState === "loading"}
    <p class="page__muted">Loading…</p>
  {:else if loadState === "error"}
    <div class="page__error" role="alert">
      <strong>Could not load margin profiles.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else if rows.length === 0}
    <div class="page__empty">
      <p>No margin profiles yet. Add your first.</p>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New profile
      </button>
    </div>
  {:else if filtered.length === 0}
    <p class="page__muted">
      No profile matches the current filter.
      {#if needle.trim().length > 0}
        <button
          type="button"
          class="quiet-button clear-filters"
          onclick={() => (needle = "")}
        >
          Clear filter
        </button>
      {/if}
    </p>
  {:else}
    <table class="profiles-table">
      <thead>
        <tr>
          <th scope="col">Name</th>
          <th scope="col">Customer type</th>
          <th scope="col">Target margin</th>
          <th scope="col">Min (floor)</th>
          <th scope="col">Enabled</th>
          <th scope="col" class="actions-header">
            <span class="visually-hidden">Actions</span>
          </th>
        </tr>
      </thead>
      <tbody>
        {#each filtered as profile (profile.id)}
          <tr>
            <td>{profile.name}</td>
            <td>
              <span class="type-chip">
                {customerTypeLabel(profile.customer_type)}
              </span>
            </td>
            <td class="mono">{formatPercent(profile.gross_margin_pct)}</td>
            <td class="mono">{formatPercent(profile.min_margin_pct)}</td>
            <td>
              {#if profile.enabled}
                <span class="enabled-chip">Yes</span>
              {:else}
                <span class="disabled-chip">No</span>
              {/if}
            </td>
            <td class="actions">
              {#if confirmArchiveId === profile.id}
                <div class="confirm">
                  <span class="confirm__text">
                    Archive <strong>{profile.name}</strong>? It stays in the
                    database for historical references.
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
                      onclick={() => void confirmArchive(profile.id)}
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
                  onclick={() => openEdit(profile)}
                >
                  Edit
                </button>
                <button
                  type="button"
                  class="quiet-button"
                  onclick={() => requestArchive(profile.id)}
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
  <MarginProfileForm
    profile={modalState === "new" ? null : modalState}
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
    width: 360px;
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

  .profiles-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }

  .profiles-table th,
  .profiles-table td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
    vertical-align: top;
  }

  .profiles-table th {
    color: var(--color-text-secondary);
    font-weight: 500;
    background: var(--color-surface-raised);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: var(--type-size-xs);
  }

  .profiles-table td.mono {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
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

  .type-chip {
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
