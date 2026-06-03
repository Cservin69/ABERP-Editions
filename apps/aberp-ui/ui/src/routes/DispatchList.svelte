<script lang="ts">
  // S234 / PR-230 / ADR-0064 — Stage 3 Phase γ Dispatch board v1 SPA
  // surface. Closes the Stage 3 → Stage 1 loop.
  //
  // ONE component drives the Dispatch board tab:
  //   - Eligible-WO side panel (Completed WOs with no prior dispatch)
  //   - State-facet filtered dispatch list (Drafted/Shipped/Cancelled/All)
  //   - Per-row Mark-Shipped modal (closed-vocab carrier picker + tracking)
  //   - Per-row Cancel button (Drafted only)
  //   - Shipped rows expose a click-through to the spawned invoice when
  //     `spawned_invoice_id !== null` (PR-230b will populate this; v1
  //     ships the noop spawner per [[pushback-as-method]] divergence,
  //     so the click-through shows a "Create invoice draft from this
  //     dispatch" affordance that pre-fills the existing IssueInvoice
  //     form via sessionStorage hand-off).
  //
  // Per CLAUDE.md rule 2 (simplicity first) + ADR-0064 §"Out of scope"
  // the v1 intentionally does NOT have:
  //   - carrier-API integration (label print, real tracking pull)
  //   - partial shipments / consolidated invoicing
  //   - returns / RMA flow
  //   - dispatch SLA / nag for "shipped but not invoiced > N days"
  //   - persistence of state filter (the WO + QA list precedents
  //     didn't have it either; lands when an operator survey demands it)
  //
  // Dark-theme posture — tokens.css only; canonical references are
  // QaList.svelte (just shipped — facet + dense table + modal frame),
  // PartnersList.svelte, IncomingInvoiceList.svelte per
  // [[spa-dark-theme-default]].

  import { onMount } from "svelte";
  import {
    cancelDispatch,
    createDispatch,
    listDispatches,
    listEligibleWorkOrders,
    listPartners,
    markDispatchShipped,
    type CarrierKind,
    type Dispatch,
    type DispatchState,
    type EligibleWorkOrder,
    type Partner,
  } from "../lib/api";

  const STATE_FACETS: { state: DispatchState | null; hu: string; en: string }[] = [
    { state: "drafted", hu: "Vázlat", en: "Drafted" },
    { state: "shipped", hu: "Kiszállítva", en: "Shipped" },
    { state: "cancelled", hu: "Visszavonva", en: "Cancelled" },
    { state: null, hu: "Mind", en: "All" },
  ];

  // Closed-vocab carrier list per ADR-0064 §1. New carriers go in by
  // enum extension; this is the SPA mirror of `aberp_dispatch::CarrierKind`.
  const CARRIER_OPTIONS: { value: CarrierKind; hu: string; en: string }[] = [
    { value: "self_delivery", hu: "Saját kiszállítás", en: "Self delivery" },
    { value: "customer_pickup", hu: "Vevő átvétel", en: "Customer pickup" },
    { value: "magyar_posta", hu: "Magyar Posta", en: "Magyar Posta" },
    { value: "gls", hu: "GLS", en: "GLS" },
    { value: "dpd", hu: "DPD", en: "DPD" },
    { value: "foxpost", hu: "Foxpost", en: "Foxpost" },
    { value: "other", hu: "Egyéb", en: "Other" },
  ];

  let rows: Dispatch[] = $state([]);
  let eligible: EligibleWorkOrder[] = $state([]);
  let partners: Partner[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);
  let selectedState: DispatchState | null = $state("drafted");
  let actionError: string | null = $state(null);
  let busyDspId: string | null = $state(null);

  // Create-dispatch modal state.
  let createOpenForWo: EligibleWorkOrder | null = $state(null);
  let createPartnerId: string = $state("");
  let createNotes: string = $state("");

  // Mark-shipped modal state.
  let shipOpenForDsp: Dispatch | null = $state(null);
  let shipCarrier: CarrierKind = $state("magyar_posta");
  let shipTracking: string = $state("");

  async function refresh(): Promise<void> {
    loadState = "loading";
    try {
      rows = await listDispatches(selectedState);
      eligible = await listEligibleWorkOrders();
      if (partners.length === 0) {
        partners = await listPartners();
      }
      loadState = "loaded";
      loadError = null;
    } catch (e: unknown) {
      loadState = "error";
      loadError = e instanceof Error ? e.message : String(e);
    }
  }

  function setFacet(s: DispatchState | null) {
    selectedState = s;
    void refresh();
  }

  function partnerLabel(id: string): string {
    const p = partners.find((x) => x.id === id);
    return p ? p.display_name : id;
  }

  function ulidIdempotencyKey(): string {
    // Same posture as other SPA forms (PartnerForm.svelte, IssueInvoice).
    // crypto.randomUUID is the standard SPA mint; the backend's F8
    // gate accepts any non-empty string.
    return (typeof crypto !== "undefined" && crypto.randomUUID
      ? crypto.randomUUID()
      : String(Date.now())
    ).replace(/-/g, "");
  }

  function openCreateForWo(wo: EligibleWorkOrder) {
    createOpenForWo = wo;
    createPartnerId = partners[0]?.id ?? "";
    createNotes = "";
    actionError = null;
  }

  function closeCreateModal() {
    createOpenForWo = null;
    createPartnerId = "";
    createNotes = "";
  }

  async function submitCreate() {
    if (!createOpenForWo || !createPartnerId) return;
    busyDspId = createOpenForWo.wo_id;
    actionError = null;
    try {
      await createDispatch({
        wo_id: createOpenForWo.wo_id,
        partner_id: createPartnerId,
        notes: createNotes.trim() === "" ? null : createNotes.trim(),
        idempotency_key: ulidIdempotencyKey(),
      });
      closeCreateModal();
      await refresh();
    } catch (e: unknown) {
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      busyDspId = null;
    }
  }

  function openShipForDsp(dsp: Dispatch) {
    shipOpenForDsp = dsp;
    shipCarrier = "magyar_posta";
    shipTracking = "";
    actionError = null;
  }

  function closeShipModal() {
    shipOpenForDsp = null;
    shipTracking = "";
  }

  async function submitShip() {
    if (!shipOpenForDsp) return;
    busyDspId = shipOpenForDsp.dsp_id;
    actionError = null;
    try {
      await markDispatchShipped(shipOpenForDsp.dsp_id, {
        carrier_kind: shipCarrier,
        tracking_number: shipTracking.trim() === "" ? null : shipTracking.trim(),
        shipped_at: null, // server stamps now()
        idempotency_key: ulidIdempotencyKey(),
      });
      closeShipModal();
      await refresh();
    } catch (e: unknown) {
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      busyDspId = null;
    }
  }

  async function cancelRow(dsp: Dispatch) {
    if (!confirm(`Cancel dispatch ${dsp.dsp_id}? No inventory impact.`)) return;
    busyDspId = dsp.dsp_id;
    actionError = null;
    try {
      await cancelDispatch(dsp.dsp_id);
      await refresh();
    } catch (e: unknown) {
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      busyDspId = null;
    }
  }

  function carrierLabel(c: CarrierKind | null): string {
    if (c === null) return "—";
    const opt = CARRIER_OPTIONS.find((o) => o.value === c);
    return opt ? opt.hu : c;
  }

  function formatTimestamp(iso: string | null): string {
    if (!iso) return "—";
    // Best-effort short form. Same posture as InvoiceList.svelte's
    // short-time formatter.
    try {
      const d = new Date(iso);
      return d.toLocaleString("hu-HU");
    } catch {
      return iso;
    }
  }

  onMount(() => {
    void refresh();
  });
</script>

<section class="dsp-page" aria-labelledby="dsp-title">
  <header class="dsp-head">
    <h2 id="dsp-title">Kiszállítás / Dispatch</h2>
    <div class="dsp-head-actions">
      <button type="button" onclick={() => refresh()}>Refresh</button>
    </div>
  </header>

  {#if loadState === "error"}
    <p class="dsp-error">Error: {loadError}</p>
  {/if}

  {#if actionError}
    <p class="dsp-error">Action failed: {actionError}</p>
  {/if}

  <div class="dsp-grid">
    <!-- LEFT: Eligible WO panel -->
    <aside class="dsp-eligible">
      <h3>Ready to dispatch — {eligible.length}</h3>
      {#if eligible.length === 0}
        <p class="dsp-empty">No Completed WOs awaiting dispatch.</p>
      {:else}
        <ul>
          {#each eligible as wo}
            <li>
              <div class="dsp-eligible__row">
                <span class="dsp-eligible__wo">{wo.wo_number}</span>
                <span class="dsp-eligible__qty">qty {wo.qty_target}</span>
              </div>
              <button
                type="button"
                class="dsp-btn dsp-btn--create"
                disabled={busyDspId !== null || partners.length === 0}
                onclick={() => openCreateForWo(wo)}
              >Create dispatch</button>
            </li>
          {/each}
        </ul>
      {/if}
    </aside>

    <!-- RIGHT: Dispatch list -->
    <div class="dsp-list">
      <div class="dsp-facets" role="tablist" aria-label="State filter">
        {#each STATE_FACETS as f}
          <button
            type="button"
            role="tab"
            class="dsp-facet"
            class:dsp-facet--active={selectedState === f.state}
            onclick={() => setFacet(f.state)}
          >
            <span class="dsp-facet__hu">{f.hu}</span>
            <span class="dsp-facet__en">{f.en}</span>
          </button>
        {/each}
      </div>

      {#if loadState === "loading"}
        <p class="dsp-empty">Loading…</p>
      {:else if rows.length === 0}
        <p class="dsp-empty">No dispatches in this view.</p>
      {:else}
        <table class="dsp-table">
          <thead>
            <tr>
              <th>Dispatch</th>
              <th>WO</th>
              <th>Partner</th>
              <th>State</th>
              <th>Carrier</th>
              <th>Tracking</th>
              <th>Shipped</th>
              <th>Invoice</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>
            {#each rows as row}
              <tr>
                <td><code>{row.dsp_id}</code></td>
                <td><code>{row.wo_id}</code></td>
                <td>{partnerLabel(row.partner_id)}</td>
                <td>
                  <span class="dsp-chip dsp-chip--{row.state}">{row.state}</span>
                </td>
                <td>{carrierLabel(row.carrier_kind)}</td>
                <td>{row.tracking_number ?? "—"}</td>
                <td>{formatTimestamp(row.shipped_at)}</td>
                <td>
                  {#if row.spawned_invoice_id}
                    <code>{row.spawned_invoice_id}</code>
                  {:else if row.state === "shipped"}
                    <span class="dsp-pending">
                      draft pending (PR-230b)
                    </span>
                  {:else}
                    —
                  {/if}
                </td>
                <td>
                  <div class="dsp-row-actions">
                    {#if row.state === "drafted"}
                      <button
                        type="button"
                        class="dsp-btn dsp-btn--ship"
                        disabled={busyDspId === row.dsp_id}
                        onclick={() => openShipForDsp(row)}
                      >Mark shipped</button>
                      <button
                        type="button"
                        class="dsp-btn dsp-btn--cancel"
                        disabled={busyDspId === row.dsp_id}
                        onclick={() => cancelRow(row)}
                      >Cancel</button>
                    {:else}
                      <span class="dsp-row-actions__note">{row.state}</span>
                    {/if}
                  </div>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </div>
  </div>
</section>

<!-- Create-dispatch modal -->
{#if createOpenForWo}
  <div
    class="dsp-modal"
    role="dialog"
    aria-labelledby="dsp-create-title"
    aria-modal="true"
  >
    <div class="dsp-modal__body">
      <h3 id="dsp-create-title">
        Create dispatch — WO {createOpenForWo.wo_number}
      </h3>
      <label>
        Recipient (partner)
        <select bind:value={createPartnerId}>
          {#each partners as p}
            <option value={p.id}>{p.display_name}</option>
          {/each}
        </select>
      </label>
      <label>
        Notes (optional)
        <textarea bind:value={createNotes}></textarea>
      </label>
      <div class="dsp-modal__actions">
        <button type="button" onclick={closeCreateModal}>Cancel</button>
        <button
          type="button"
          class="dsp-btn--create-confirm"
          disabled={!createPartnerId || busyDspId !== null}
          onclick={submitCreate}
        >Create</button>
      </div>
    </div>
  </div>
{/if}

<!-- Mark-shipped modal -->
{#if shipOpenForDsp}
  <div
    class="dsp-modal"
    role="dialog"
    aria-labelledby="dsp-ship-title"
    aria-modal="true"
  >
    <div class="dsp-modal__body">
      <h3 id="dsp-ship-title">Mark shipped — {shipOpenForDsp.dsp_id}</h3>
      <p class="dsp-modal__warn">
        This decrements stock by {partnerLabel(shipOpenForDsp.partner_id)}'s WO qty
        AND spawns an invoice draft in the SAME transaction (or deferred to
        PR-230b's spawner). Any failure rolls back the entire change.
      </p>
      <label>
        Carrier
        <select bind:value={shipCarrier}>
          {#each CARRIER_OPTIONS as opt}
            <option value={opt.value}>{opt.hu} — {opt.en}</option>
          {/each}
        </select>
      </label>
      <label>
        Tracking number (optional)
        <input type="text" bind:value={shipTracking} placeholder="MPL-123-XYZ" />
      </label>
      <div class="dsp-modal__actions">
        <button type="button" onclick={closeShipModal}>Cancel</button>
        <button
          type="button"
          class="dsp-btn--ship-confirm"
          disabled={busyDspId !== null}
          onclick={submitShip}
        >Confirm ship</button>
      </div>
    </div>
  </div>
{/if}

<style>
  /* S234 / PR-230 / ADR-0064 — Dispatch board v1 dark-theme styles.
     Tokens only; no hardcoded hex. Canonical references per
     [[spa-dark-theme-default]]: QaList.svelte (facet + dense table +
     modal frame), PartnersList.svelte, IncomingInvoiceList.svelte. */

  .dsp-page {
    padding: var(--space-4);
    color: var(--color-text-primary);
  }

  .dsp-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }

  .dsp-head h2 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .dsp-head-actions button {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .dsp-head-actions button:hover {
    border-color: var(--color-text-muted);
  }

  .dsp-grid {
    display: grid;
    grid-template-columns: 280px 1fr;
    gap: var(--space-4);
  }

  .dsp-eligible {
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-3);
  }

  .dsp-eligible h3 {
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
    font-weight: 500;
  }

  .dsp-eligible ul {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .dsp-eligible li {
    background: var(--color-surface-raised);
    padding: var(--space-2);
    border-radius: 4px;
    border: 1px solid var(--color-surface-divider);
  }

  .dsp-eligible__row {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
    margin-bottom: var(--space-2);
  }

  .dsp-eligible__wo {
    font-weight: 500;
  }

  .dsp-eligible__qty {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }

  .dsp-list {
    min-width: 0;
  }

  .dsp-facets {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }

  .dsp-facet {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .dsp-facet:hover {
    color: var(--color-text-strong);
  }

  .dsp-facet--active {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .dsp-facet__hu {
    display: block;
    font-weight: 600;
  }

  .dsp-facet__en {
    display: block;
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .dsp-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
    background: var(--color-surface-sunken);
  }

  .dsp-table thead th {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    font-weight: 500;
  }

  .dsp-table tbody td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
  }

  .dsp-table tbody tr:hover {
    background: var(--color-surface-raised);
  }

  .dsp-table code {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .dsp-chip {
    display: inline-block;
    padding: 2px var(--space-2);
    border-radius: 3px;
    font-size: var(--type-size-xs);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
  }

  .dsp-chip--drafted {
    border-color: var(--color-signal-warning);
    color: var(--color-text-strong);
  }

  .dsp-chip--shipped {
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
    border-color: var(--color-signal-positive);
    font-weight: 500;
  }

  .dsp-chip--cancelled {
    color: var(--color-text-muted);
    border-color: var(--color-signal-muted);
    background: var(--color-surface-raised);
  }

  .dsp-pending {
    color: var(--color-text-muted);
    font-style: italic;
    font-size: var(--type-size-xs);
  }

  .dsp-row-actions {
    display: flex;
    gap: var(--space-1);
  }

  .dsp-row-actions__note {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-style: italic;
  }

  .dsp-btn {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    border-radius: 4px;
    cursor: pointer;
  }

  .dsp-btn:hover:not(:disabled) {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .dsp-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .dsp-btn--ship:hover:not(:disabled),
  .dsp-btn--create:hover:not(:disabled) {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  .dsp-btn--cancel:hover:not(:disabled) {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .dsp-error {
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm);
  }

  .dsp-empty {
    color: var(--color-text-muted);
    padding: var(--space-4);
    border: 1px dashed var(--color-surface-divider);
    background: var(--color-surface-raised);
    text-align: center;
    border-radius: 4px;
    font-size: var(--type-size-sm);
  }

  /* Modal — mirrors QaList.svelte's dispose-confirm modal frame. */
  .dsp-modal {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }

  .dsp-modal__body {
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-4) var(--space-5);
    border-radius: 4px;
    max-width: 500px;
    width: 90%;
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .dsp-modal__body h3 {
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .dsp-modal__warn {
    background: var(--color-surface-raised);
    border-left: 3px solid var(--color-signal-warning);
    padding: var(--space-2) var(--space-3);
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  .dsp-modal__body label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-bottom: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  .dsp-modal__body label input,
  .dsp-modal__body label select,
  .dsp-modal__body label textarea {
    display: block;
    width: 100%;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }

  .dsp-modal__body label textarea {
    min-height: 4em;
    resize: vertical;
  }

  .dsp-modal__actions {
    margin-top: var(--space-3);
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .dsp-modal__actions button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2) var(--space-4);
    border-radius: 4px;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .dsp-modal__actions button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .dsp-modal__actions button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .dsp-modal__actions button.dsp-btn--ship-confirm,
  .dsp-modal__actions button.dsp-btn--create-confirm {
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
    border-color: var(--color-signal-positive);
    font-weight: 500;
  }
</style>
