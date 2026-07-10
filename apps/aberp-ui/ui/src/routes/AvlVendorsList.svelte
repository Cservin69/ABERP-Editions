<script lang="ts">
  // S431 — Approved Vendor List (AVL) master-data screen.
  //   1. Open #/avl-vendors. Page lists every vendor (incl. revoked).
  //   2. "+ New vendor" / "Edit" → AvlVendorForm modal.
  //   3. "Screen" → AvlScreenVendorModal (records supplier.export_screened).
  //   4. Change status via the per-row select; "revoked" needs a reason
  //      (inline confirm). Reactivating a revoked vendor needs an explicit
  //      override — the backend refuses the normal transition (409), and the
  //      row offers a "Force reactivate" action.

  import { onMount } from "svelte";
  import {
    listAvlVendors,
    setAvlVendorStatus,
    type ApprovedStatus,
    type AvlVendor,
  } from "../lib/api";
  import {
    APPROVED_STATUSES,
    categoryLabel,
    EMPTY_VENDOR_FILTER,
    filterVendors,
    isVendorFilterEmpty,
    statusChipClass,
    statusLabel,
    vendorIsOverdue,
    type StatusFacet,
    type VendorFilterSpec,
  } from "../lib/avl-vendors";
  import AvlVendorForm from "./AvlVendorForm.svelte";
  import AvlScreenVendorModal from "./AvlScreenVendorModal.svelte";

  let rows: AvlVendor[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  let filter: VendorFilterSpec = $state({ ...EMPTY_VENDOR_FILTER });

  // Modal state for create/edit + screen.
  let formState: "new" | AvlVendor | null = $state(null);
  let screenVendor: AvlVendor | null = $state(null);

  // Per-row revoke confirm (vendor id) + reason + error.
  let revokeId: string | null = $state(null);
  let revokeReason = $state("");
  let rowError: { id: string; message: string; canForce: boolean } | null = $state(null);

  // `now` for the overdue chip — sampled once at mount (the list is a
  // snapshot; a refresh re-samples).
  let now = $state(new Date(0));

  let filtered = $derived(filterVendors(rows, filter));

  const STATUS_FACETS: readonly { value: StatusFacet; label: string }[] = [
    { value: "All", label: "All" },
    ...APPROVED_STATUSES,
  ];

  onMount(() => {
    void loadVendors();
  });

  async function loadVendors() {
    loadState = "loading";
    loadError = null;
    try {
      rows = await listAvlVendors();
      now = new Date();
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function openCreate() {
    formState = "new";
  }
  function openEdit(v: AvlVendor) {
    formState = v;
  }
  async function onSaved() {
    formState = null;
    await loadVendors();
  }
  async function onScreened() {
    screenVendor = null;
    await loadVendors();
  }

  function clearRowError() {
    rowError = null;
  }

  // A status change from the per-row select. "revoked" routes to the
  // inline reason confirm; every other target calls the backend directly
  // and surfaces a 409 (e.g. from a revoked row) with a Force action.
  async function changeStatus(v: AvlVendor, next: ApprovedStatus, force = false) {
    clearRowError();
    if (next === "revoked" && !force) {
      revokeId = v.id;
      revokeReason = "";
      return;
    }
    try {
      await setAvlVendorStatus(v.id, next, null, force);
      await loadVendors();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      // A 409 conflict on a normal transition means a manual override is
      // required (e.g. reactivating a revoked vendor).
      rowError = { id: v.id, message, canForce: !force };
    }
  }

  function onStatusSelect(v: AvlVendor, event: Event) {
    const next = (event.currentTarget as HTMLSelectElement).value as ApprovedStatus;
    if (next === v.approved_status) return;
    void changeStatus(v, next);
  }

  async function confirmRevoke(v: AvlVendor) {
    clearRowError();
    try {
      await setAvlVendorStatus(v.id, "revoked", revokeReason, false);
      revokeId = null;
      revokeReason = "";
      await loadVendors();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      rowError = { id: v.id, message, canForce: false };
    }
  }

  function cancelRevoke() {
    revokeId = null;
    revokeReason = "";
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Approved Vendor List</h2>
      <button type="button" class="page__primary" onclick={openCreate}>
        + New vendor
      </button>
    </div>
    <p class="page__lede">
      Vendors and their approval status, categories and re-screening window.
      Suspended / revoked vendors are refused at PO time. Screening records an
      audit event (live denied-party integration is future work).
    </p>
  </header>

  <div class="page__toolbar">
    <label class="page__search">
      <span class="visually-hidden">Filter vendors</span>
      <input
        type="search"
        value={filter.needle}
        oninput={(e) =>
          (filter = { ...filter, needle: (e.currentTarget as HTMLInputElement).value })}
        placeholder="Filter by partner id…"
        autocomplete="off"
        spellcheck="false"
      />
    </label>
    <label class="filter">
      <span class="filter-label">Status</span>
      <select
        value={filter.status}
        onchange={(e) =>
          (filter = { ...filter, status: (e.currentTarget as HTMLSelectElement).value as StatusFacet })}
        aria-label="Filter vendors by status"
      >
        {#each STATUS_FACETS as option (option.value)}
          <option value={option.value}>{option.label}</option>
        {/each}
      </select>
    </label>
    <button type="button" class="quiet-button" onclick={() => void loadVendors()}>
      Refresh
    </button>
  </div>

  {#if loadState === "loading"}
    <p class="page__muted">Loading…</p>
  {:else if loadState === "error"}
    <div class="page__error" role="alert">
      <strong>Could not load vendors.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else if rows.length === 0}
    <div class="page__empty">
      <p>No vendors yet. Add your first.</p>
      <button type="button" class="page__primary" onclick={openCreate}>+ New vendor</button>
    </div>
  {:else if filtered.length === 0}
    <p class="page__muted">
      No vendor matches the current filter.
      {#if !isVendorFilterEmpty(filter)}
        <button
          type="button"
          class="quiet-button clear-filters"
          onclick={() => (filter = { ...EMPTY_VENDOR_FILTER })}
        >
          Clear filters
        </button>
      {/if}
    </p>
  {:else}
    <table class="vendors-table">
      <thead>
        <tr>
          <th scope="col">Vendor (partner)</th>
          <th scope="col">Status</th>
          <th scope="col">Categories</th>
          <th scope="col">Approved until</th>
          <th scope="col">Reviewer</th>
          <th scope="col" class="actions-header"><span class="visually-hidden">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each filtered as v (v.id)}
          <tr>
            <td class="mono">{v.partner_id}</td>
            <td>
              <span class={statusChipClass(v.approved_status)}>{statusLabel(v.approved_status)}</span>
              {#if vendorIsOverdue(v, now)}
                <span class="chip chip--warning" title="Re-screening overdue">overdue</span>
              {/if}
            </td>
            <td class="cats">
              {#if v.approval_categories.length === 0}
                <span class="page__muted">—</span>
              {:else}
                {#each v.approval_categories as c (c)}
                  <span class="cat-chip">{categoryLabel(c)}</span>
                {/each}
              {/if}
            </td>
            <td class="mono">{v.approved_until_utc ?? "—"}</td>
            <td class="mono">{v.reviewer_login}</td>
            <td class="actions">
              {#if revokeId === v.id}
                <div class="confirm">
                  <span class="confirm__text">
                    Revoke <strong>{v.partner_id}</strong>? The row stays on the
                    AVL; a reason is required.
                  </span>
                  <input
                    type="text"
                    class="reason-input"
                    bind:value={revokeReason}
                    placeholder="Revocation reason…"
                    autocomplete="off"
                  />
                  <div class="confirm__buttons">
                    <button type="button" class="quiet-button" onclick={cancelRevoke}>Cancel</button>
                    <button
                      type="button"
                      class="quiet-button danger"
                      disabled={revokeReason.trim() === ""}
                      onclick={() => void confirmRevoke(v)}
                    >
                      Revoke
                    </button>
                  </div>
                </div>
              {:else}
                <div class="row-actions">
                  <label class="status-select">
                    <span class="visually-hidden">Change status</span>
                    <select value={v.approved_status} onchange={(e) => onStatusSelect(v, e)}>
                      {#each APPROVED_STATUSES as s (s.value)}
                        <option value={s.value}>{s.label}</option>
                      {/each}
                    </select>
                  </label>
                  <button type="button" class="quiet-button" onclick={() => openEdit(v)}>Edit</button>
                  <button type="button" class="quiet-button" onclick={() => (screenVendor = v)}>
                    Screen
                  </button>
                </div>
                {#if rowError !== null && rowError.id === v.id}
                  <div class="confirm">
                    <p class="confirm__error" role="alert">{rowError.message}</p>
                    {#if rowError.canForce}
                      <div class="confirm__buttons">
                        <button type="button" class="quiet-button" onclick={clearRowError}>
                          Dismiss
                        </button>
                        <button
                          type="button"
                          class="quiet-button danger"
                          onclick={() => void changeStatus(v, "approved", true)}
                        >
                          Force reactivate (override)
                        </button>
                      </div>
                    {/if}
                  </div>
                {/if}
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if formState !== null}
  <AvlVendorForm
    vendor={formState === "new" ? null : formState}
    onSaved={onSaved}
    onClose={() => (formState = null)}
  />
{/if}

{#if screenVendor !== null}
  <AvlScreenVendorModal
    vendor={screenVendor}
    onScreened={onScreened}
    onClose={() => (screenVendor = null)}
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

  .vendors-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }

  .vendors-table th,
  .vendors-table td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
    vertical-align: top;
  }

  .vendors-table th {
    color: var(--color-text-secondary);
    font-weight: 500;
    background: var(--color-surface-raised);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: var(--type-size-xs);
  }

  .vendors-table td.mono {
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

  .filter select,
  .status-select select {
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
  }

  .row-actions {
    display: flex;
    gap: var(--space-2);
    align-items: center;
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

  .quiet-button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .quiet-button.danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .chip {
    display: inline-block;
    padding: 0 var(--space-2);
    border-radius: var(--radius-lg);
    border: 1px solid var(--color-surface-divider);
    font-size: var(--type-size-xs);
    font-weight: 500;
  }

  .chip--ok {
    color: var(--color-signal-positive, var(--color-text-strong));
    border-color: var(--color-signal-positive, var(--color-text-strong));
  }

  .chip--err {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .chip--warning {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
    margin-left: var(--space-1);
  }

  .chip--neutral {
    color: var(--color-text-muted);
  }

  .cat-chip {
    display: inline-block;
    padding: 0 var(--space-2);
    margin: 0 var(--space-1) var(--space-1) 0;
    border-radius: var(--radius-lg);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-size: var(--type-size-xs);
  }

  .cats {
    white-space: normal;
    max-width: 240px;
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
    margin-top: var(--space-2);
  }

  .confirm__text {
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .confirm__text strong {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }

  .reason-input {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
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
