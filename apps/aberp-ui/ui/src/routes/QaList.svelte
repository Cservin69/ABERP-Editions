<script lang="ts">
  // S233 / PR-229 / ADR-0063 — Stage 3 Phase γ QA queue v1 SPA surface.
  //
  // ONE component drives the QA queue tab: a state-facet list view +
  // per-row Pass/Fail/Rework/Dispose actions. Dispose surfaces a
  // confirm modal naming the scrap qty before the POST fires.
  //
  // Per CLAUDE.md rule 2 (simplicity first) + ADR-0063 §8 the v1
  // intentionally does NOT have:
  //   - measurement-blob structured viewer (the `measurement` column
  //     carries raw strings only in v1 — adapter blobs in v2)
  //   - persistence of state filter (the WO list precedent didn't have
  //     it either; lands when an operator survey demands it)
  //   - per-decision "are you sure?" beyond Dispose (Pass/Fail/Rework
  //     are reversible via the next decision in the same UI)
  //
  // Dark-theme posture — tokens.css only; canonical references are
  // PartnersList.svelte / IncomingInvoiceList.svelte / PartnerForm.svelte
  // per [[spa-dark-theme-default]].

  import { onMount } from "svelte";
  import {
    decideQaInspection,
    getWorkOrder,
    listQaInspections,
    listProducts,
    type QaDecision,
    type QaInspection,
    type QaState,
    type Product,
    type WorkOrderDetailResponse,
  } from "../lib/api";

  const STATE_FACETS: { state: QaState | null; hu: string; en: string }[] = [
    { state: "pending", hu: "Függő", en: "Pending" },
    { state: "passed", hu: "Átment", en: "Passed" },
    { state: "failed", hu: "Bukott", en: "Failed" },
    { state: "reworking", hu: "Újragyártás", en: "Reworking" },
    { state: "disposed", hu: "Selejt", en: "Disposed" },
    { state: null, hu: "Mind", en: "All" },
  ];

  let rows: QaInspection[] = $state([]);
  let products: Product[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);
  let selectedState: QaState | null = $state("pending");

  let actionError: string | null = $state(null);
  let busyQaId: string | null = $state(null);

  // WO-context cache so the row can show "WO# — Op name" without a
  // re-render-on-mount fetch per row. Keyed by wo_id.
  let woContextCache: Record<string, WorkOrderDetailResponse> = $state({});

  // Dispose-confirm modal state. Names the qa_id + estimated scrap
  // qty so the operator can't fat-finger a 10-unit scrap.
  let disposeConfirmQaId: string | null = $state(null);
  let disposeConfirmReason: string = $state("");
  let disposeConfirmScrapQty: string = $state("");

  async function refresh(): Promise<void> {
    loadState = "loading";
    try {
      rows = await listQaInspections(selectedState);
      if (products.length === 0) {
        products = await listProducts();
      }
      // Pre-fetch WO context for the rows we display (cap at 50 to
      // avoid a fan-out storm — see [[trust-code-not-operator]]).
      const woIds = Array.from(
        new Set(rows.slice(0, 50).map((r) => r.wo_id)),
      ).filter((id) => woContextCache[id] === undefined);
      for (const id of woIds) {
        try {
          const detail = await getWorkOrder(id);
          woContextCache = { ...woContextCache, [id]: detail };
        } catch {
          // Best-effort — a missing context just falls back to the
          // raw id display.
        }
      }
      loadState = "loaded";
      loadError = null;
    } catch (e) {
      loadState = "error";
      loadError = String(e);
    }
  }

  function setStateFilter(s: QaState | null): void {
    selectedState = s;
    refresh();
  }

  function mintIdempotencyKey(prefix: string): string {
    if (
      typeof globalThis !== "undefined" &&
      globalThis.crypto?.randomUUID
    ) {
      return `${prefix}-${globalThis.crypto.randomUUID()}`;
    }
    return `${prefix}-${Date.now().toString(36)}-${Math.random()
      .toString(36)
      .slice(2, 10)}`;
  }

  function woNumber(qa: QaInspection): string {
    return woContextCache[qa.wo_id]?.work_order.wo_number ?? qa.wo_id;
  }

  function productName(qa: QaInspection): string {
    const wo = woContextCache[qa.wo_id]?.work_order;
    if (wo === undefined) return "—";
    const p = products.find((p) => p.id === wo.product_id);
    return p?.name ?? wo.product_id;
  }

  function opName(qa: QaInspection): string {
    const wo = woContextCache[qa.wo_id];
    if (wo === undefined) return qa.routing_op_id;
    const op = wo.routing_ops.find((o) => o.routing_op_id === qa.routing_op_id);
    return op?.op_name ?? qa.routing_op_id;
  }

  function woScrapQty(qa: QaInspection): string {
    const wo = woContextCache[qa.wo_id]?.work_order;
    return wo?.qty_target ?? "?";
  }

  function allowedDecisions(state: QaState): QaDecision[] {
    // Mirror of `aberp_qa::state::next_qa_state` — buttons render
    // only for actions whose `from` state is the current state. A
    // curl bypassing this still gets refused loud by the backend.
    switch (state) {
      case "pending":
        return ["pass", "fail"];
      case "passed":
        return ["fail"]; // operator after-the-fact catch
      case "failed":
        return ["rework", "dispose"];
      case "reworking":
        return ["pass", "dispose"];
      case "disposed":
        return [];
    }
  }

  async function submitDecision(
    qa: QaInspection,
    decision: QaDecision,
  ): Promise<void> {
    actionError = null;
    if (decision === "dispose") {
      // Route through the confirm modal instead of POSTing immediately.
      disposeConfirmQaId = qa.qa_id;
      disposeConfirmReason = "";
      disposeConfirmScrapQty = woScrapQty(qa);
      return;
    }
    let reason: string | null = null;
    if (decision === "fail" || decision === "rework") {
      const r = window.prompt(
        decision === "fail"
          ? "Hiba oka? / Failure reason?"
          : "Újragyártás oka? / Rework reason?",
      );
      // Cancel on prompt = abort the action entirely.
      if (r === null) return;
      reason = r.trim() === "" ? null : r;
    }
    await postDecision(qa.qa_id, decision, reason, null);
  }

  async function postDecision(
    qaId: string,
    decision: QaDecision,
    reason: string | null,
    measurement: string | null,
  ): Promise<void> {
    busyQaId = qaId;
    try {
      await decideQaInspection(qaId, {
        decision,
        reason,
        measurement,
        idempotency_key: mintIdempotencyKey(`qa-${decision}-${qaId}`),
      });
      await refresh();
    } catch (e) {
      actionError = String(e);
    } finally {
      busyQaId = null;
    }
  }

  async function confirmDispose(): Promise<void> {
    if (disposeConfirmQaId === null) return;
    const reason =
      disposeConfirmReason.trim() === "" ? null : disposeConfirmReason.trim();
    const qaId = disposeConfirmQaId;
    disposeConfirmQaId = null;
    await postDecision(qaId, "dispose", reason, null);
  }

  function cancelDispose(): void {
    disposeConfirmQaId = null;
    disposeConfirmReason = "";
  }

  function stateChipLabel(s: QaState): string {
    const f = STATE_FACETS.find((f) => f.state === s);
    return f?.hu ?? s;
  }

  onMount(refresh);
</script>

<section class="qa-page" aria-labelledby="qa-title">
  <header class="qa-head">
    <h2 id="qa-title">Minőség / QA queue</h2>
    <div class="qa-head-actions">
      <button type="button" onclick={refresh}>Frissítés / Refresh</button>
    </div>
  </header>

  <div class="qa-facets" role="tablist" aria-label="State filter">
    {#each STATE_FACETS as f}
      <button
        type="button"
        class="qa-facet"
        class:qa-facet--active={selectedState === f.state}
        onclick={() => setStateFilter(f.state)}
      >
        <span class="qa-facet__hu">{f.hu}</span>
        <span class="qa-facet__en">{f.en}</span>
      </button>
    {/each}
  </div>

  {#if loadState === "loading"}
    <p>Loading…</p>
  {:else if loadState === "error"}
    <p class="qa-error">Error: {loadError}</p>
  {:else if rows.length === 0}
    <p class="qa-empty">
      No inspections in this state. The QA queue fills as routing
      operations complete on released work orders.
    </p>
  {:else}
    {#if actionError !== null}
      <p class="qa-error">Decision failed: {actionError}</p>
    {/if}
    <table class="qa-table">
      <thead>
        <tr>
          <th>WO #</th>
          <th>Termék / Product</th>
          <th>Művelet / Op</th>
          <th>Állapot / State</th>
          <th>Létrehozva / Created</th>
          <th>Döntő / Decided by</th>
          <th>Megjegyzés / Note</th>
          <th>Műveletek / Actions</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row}
          <tr class:qa-row--superseded={row.superseded_by !== null}>
            <td>{woNumber(row)}</td>
            <td>{productName(row)}</td>
            <td>{opName(row)}</td>
            <td>
              <span class="qa-chip qa-chip--{row.state}">
                {stateChipLabel(row.state)}
              </span>
            </td>
            <td>{row.created_at}</td>
            <td>{row.decided_by ?? "—"}</td>
            <td>{row.reason ?? "—"}</td>
            <td>
              <div class="qa-row-actions">
                {#if row.superseded_by !== null}
                  <span class="qa-row-actions__note">superseded</span>
                {:else}
                  {#each allowedDecisions(row.state) as d}
                    <button
                      type="button"
                      class="qa-btn qa-btn--{d}"
                      onclick={() => submitDecision(row, d)}
                      disabled={busyQaId === row.qa_id}
                    >
                      {#if d === "pass"}
                        ✓ Pass
                      {:else if d === "fail"}
                        ✗ Fail
                      {:else if d === "rework"}
                        ↻ Rework
                      {:else}
                        ✕ Dispose
                      {/if}
                    </button>
                  {/each}
                {/if}
              </div>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if disposeConfirmQaId !== null}
    <div
      class="qa-modal"
      role="dialog"
      aria-labelledby="qa-dispose-confirm-title"
    >
      <div class="qa-modal__body">
        <h3 id="qa-dispose-confirm-title">Selejtezés megerősítése / Confirm scrap</h3>
        <p class="qa-modal__warn">
          This will write off
          <strong>{disposeConfirmScrapQty}</strong>
          units of the finished good to scrap (a `Scrap` stock movement
          fires immediately, the WO can no longer be Completed — only
          Cancelled). Continue?
        </p>
        <label>
          Ok / Reason (optional)
          <textarea bind:value={disposeConfirmReason}></textarea>
        </label>
        <div class="qa-modal__actions">
          <button type="button" onclick={cancelDispose}>Mégse / Cancel</button>
          <button
            type="button"
            class="qa-btn--dispose-confirm"
            onclick={confirmDispose}
          >
            Selejtezés / Dispose
          </button>
        </div>
      </div>
    </div>
  {/if}
</section>

<style>
  /* S233 / PR-229 / ADR-0063 — QA queue v1 dark-theme styles. Tokens
     only; no hardcoded hex. References: WorkOrdersList.svelte (facet +
     dense table), PartnerForm.svelte (modal frame), IncomingInvoiceList
     (status chip palette per [[spa-dark-theme-default]]). */

  .qa-page {
    padding: var(--space-4);
    color: var(--color-text-primary);
  }

  .qa-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }

  .qa-head h2 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .qa-head-actions button {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .qa-head-actions button:hover {
    border-color: var(--color-text-muted);
  }

  .qa-facets {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }

  .qa-facet {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .qa-facet:hover {
    color: var(--color-text-strong);
  }

  .qa-facet--active {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .qa-facet__hu {
    display: block;
    font-weight: 600;
  }

  .qa-facet__en {
    display: block;
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .qa-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
    background: var(--color-surface-sunken);
  }

  .qa-table thead th {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    font-weight: 500;
  }

  .qa-table tbody td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
  }

  .qa-table tbody tr:hover {
    background: var(--color-surface-raised);
  }

  .qa-row--superseded {
    opacity: 0.55;
  }

  .qa-chip {
    display: inline-block;
    padding: 2px var(--space-2);
    border-radius: 3px;
    font-size: var(--type-size-xs);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
  }

  .qa-chip--pending {
    border-color: var(--color-signal-warning);
    color: var(--color-text-strong);
  }

  .qa-chip--passed {
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
    border-color: var(--color-signal-positive);
    font-weight: 500;
  }

  .qa-chip--failed,
  .qa-chip--disposed {
    background: var(--color-surface-raised);
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .qa-chip--reworking {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  .qa-row-actions {
    display: flex;
    gap: var(--space-1);
  }

  .qa-row-actions__note {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-style: italic;
  }

  .qa-btn {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    border-radius: 4px;
    cursor: pointer;
  }

  .qa-btn:hover:not(:disabled) {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .qa-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .qa-btn--pass:hover:not(:disabled) {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  .qa-btn--fail:hover:not(:disabled),
  .qa-btn--dispose:hover:not(:disabled) {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .qa-btn--rework:hover:not(:disabled) {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  .qa-error {
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm);
  }

  .qa-empty {
    color: var(--color-text-muted);
    padding: var(--space-5);
    border: 1px dashed var(--color-surface-divider);
    background: var(--color-surface-raised);
    text-align: center;
    border-radius: 4px;
  }

  /* Dispose-confirm modal — mirrors PartnerForm.svelte's dialog frame. */
  .qa-modal {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }

  .qa-modal__body {
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-4) var(--space-5);
    border-radius: 4px;
    max-width: 500px;
    width: 90%;
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .qa-modal__body h3 {
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .qa-modal__warn {
    background: var(--color-surface-raised);
    border-left: 3px solid var(--color-signal-negative);
    padding: var(--space-2) var(--space-3);
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  .qa-modal__body label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-bottom: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  .qa-modal__body label textarea {
    display: block;
    width: 100%;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    min-height: 4em;
    resize: vertical;
  }

  .qa-modal__actions {
    margin-top: var(--space-3);
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .qa-modal__actions button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2) var(--space-4);
    border-radius: 4px;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .qa-modal__actions button:hover {
    color: var(--color-text-strong);
  }

  .qa-modal__actions button.qa-btn--dispose-confirm {
    background: var(--color-signal-negative);
    color: var(--color-surface-base);
    border-color: var(--color-signal-negative);
    font-weight: 500;
  }
</style>
