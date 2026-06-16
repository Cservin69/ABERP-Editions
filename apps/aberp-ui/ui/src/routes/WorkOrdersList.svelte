<script lang="ts">
  // S232 / PR-228 / ADR-0062 — Stage 3 Phase γ Work Orders v1 SPA
  // surface.
  //
  // ONE component does double duty: a list view with state-facet
  // chips + a per-row drill-down to a detail panel showing the WO,
  // its routing operations, the active BOM snapshot, and the
  // state-aware action buttons. Keeps the route surface narrow
  // (single `#/work-orders` slug) for v1 per the ADR-0062 §"SPA
  // surface" defaults; a future deep-linkable detail-by-id can split
  // when the operator needs it.
  //
  // Per CLAUDE.md rule 2 (simplicity first) + ADR-0062's "v1 is the
  // list + the action buttons" cap, this screen deliberately does
  // NOT have:
  //   - sort/filter columns beyond the state-facet chips
  //   - persistence (the inventory v1 didn't have it either; lands
  //     when a real operator survey demands it)
  //   - a Gantt / shop-floor board
  //   - per-routing-op start/complete buttons (per-op cascade is
  //     auto per ADR-0062 §2; v1 surfaces the state)

  import { onMount } from "svelte";
  import {
    createWorkOrder,
    getWorkOrder,
    listQaInspections,
    listWorkOrders,
    transitionRoutingOp,
    transitionWorkOrder,
    listProducts,
    type Product,
    type QaInspection,
    type RoutingOp,
    type WorkOrder,
    type WorkOrderDetailResponse,
    type WorkOrderState,
    type WoAction,
  } from "../lib/api";
  import MarkPartsModal from "./MarkPartsModal.svelte";

  const STATE_FACETS: { state: WorkOrderState | null; hu: string; en: string }[] =
    [
      { state: null, hu: "Mind", en: "All" },
      { state: "created", hu: "Létrehozva", en: "Created" },
      { state: "released", hu: "Kiadva", en: "Released" },
      { state: "in_progress", hu: "Folyamatban", en: "In progress" },
      { state: "on_hold", hu: "Várakozik", en: "On hold" },
      { state: "completed", hu: "Kész", en: "Completed" },
      { state: "cancelled", hu: "Megszakítva", en: "Cancelled" },
    ];

  let rows: WorkOrder[] = $state([]);
  let products: Product[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);
  let selectedState: WorkOrderState | null = $state(null);

  let detail: WorkOrderDetailResponse | null = $state(null);
  let detailError: string | null = $state(null);
  let detailLoading = $state(false);
  let actionError: string | null = $state(null);
  let warningsToShow: string[] = $state([]);
  // S233 / PR-229 — per-op live QA inspections, keyed by routing_op_id.
  // Populated alongside `detail` so the per-op chip renders without a
  // re-fetch storm.
  let qaByOp: Record<string, QaInspection | null> = $state({});
  let opBusyId: string | null = $state(null);

  // S438 — Mark-parts modal state.
  let showMarkModal = $state(false);

  async function onPartsMarked(): Promise<void> {
    showMarkModal = false;
    if (detail !== null) await openDetail(detail.work_order.wo_id);
  }

  // Create-WO modal state.
  let showCreateForm = $state(false);
  let formWoNumber = $state("");
  let formProductId = $state("");
  let formQtyTarget = $state("1");
  let formNotes = $state("");
  let formOps: { op_name: string; est_time_min: string; est_cost_huf: string }[] =
    $state([{ op_name: "", est_time_min: "", est_cost_huf: "" }]);
  let createError: string | null = $state(null);
  let creating = $state(false);

  async function refresh(): Promise<void> {
    loadState = "loading";
    try {
      rows = await listWorkOrders(selectedState);
      if (products.length === 0) {
        products = await listProducts();
      }
      loadState = "loaded";
      loadError = null;
    } catch (e) {
      loadState = "error";
      loadError = String(e);
    }
  }

  async function openDetail(woId: string): Promise<void> {
    detailLoading = true;
    detailError = null;
    actionError = null;
    warningsToShow = [];
    try {
      detail = await getWorkOrder(woId);
      // S233 / PR-229 — fetch live QA inspections for the WO. We list
      // all inspections then index by routing_op_id (live = the row
      // where `superseded_by IS NULL`). The list call is bounded;
      // for v1 a WO has ≤200 ops × ≤N inspections = manageable.
      const allQa = await listQaInspections(null);
      const live: Record<string, QaInspection | null> = {};
      for (const op of detail.routing_ops) {
        live[op.routing_op_id] = null;
      }
      for (const qa of allQa) {
        if (qa.wo_id !== detail.work_order.wo_id) continue;
        if (qa.superseded_by !== null) continue;
        live[qa.routing_op_id] = qa;
      }
      qaByOp = live;
    } catch (e) {
      detailError = String(e);
      detail = null;
      qaByOp = {};
    } finally {
      detailLoading = false;
    }
  }

  async function completeOp(op: RoutingOp): Promise<void> {
    if (detail === null) return;
    actionError = null;
    opBusyId = op.routing_op_id;
    try {
      await transitionRoutingOp(detail.work_order.wo_id, op.routing_op_id, {
        action: "complete",
        idempotency_key: mintIdempotencyKey(
          `op-complete-${op.routing_op_id}`,
        ),
      });
      await openDetail(detail.work_order.wo_id);
    } catch (e) {
      actionError = String(e);
    } finally {
      opBusyId = null;
    }
  }

  function qaChipLabel(qa: QaInspection | null): string {
    if (qa === null) return "—";
    switch (qa.state) {
      case "pending":
        return "QA: függő";
      case "passed":
        return "QA: ✓";
      case "failed":
        return "QA: ✗";
      case "reworking":
        return "QA: ↻";
      case "disposed":
        return "QA: selejt";
    }
  }

  function closeDetail(): void {
    detail = null;
    detailError = null;
    actionError = null;
    warningsToShow = [];
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

  async function submitTransition(action: WoAction): Promise<void> {
    if (detail === null) return;
    actionError = null;
    warningsToShow = [];
    const reason =
      action === "cancel" || action === "hold"
        ? window.prompt(
            action === "hold"
              ? "Okot adj meg (kötelező OnHold-hoz) / Reason (required for Hold)"
              : "Megszakítás oka? / Cancellation reason?",
          )
        : null;
    if (action === "hold" && (reason === null || reason.trim() === "")) {
      actionError = "Hold requires a reason";
      return;
    }
    if (reason === null && action === "cancel") {
      // Operator pressed Cancel on the prompt — treat as abort.
      return;
    }
    try {
      const resp = await transitionWorkOrder(detail.work_order.wo_id, {
        action,
        reason: reason,
        idempotency_key: mintIdempotencyKey(`${action}-${detail.work_order.wo_id}`),
      });
      if (resp.warnings && resp.warnings.length > 0) {
        warningsToShow = resp.warnings;
      }
      // Refresh the detail + the list to reflect the new state.
      await openDetail(detail.work_order.wo_id);
      await refresh();
    } catch (e) {
      actionError = String(e);
    }
  }

  function addOpRow(): void {
    formOps = [...formOps, { op_name: "", est_time_min: "", est_cost_huf: "" }];
  }

  function removeOpRow(idx: number): void {
    formOps = formOps.filter((_, i) => i !== idx);
    if (formOps.length === 0) addOpRow();
  }

  async function submitCreate(): Promise<void> {
    createError = null;
    if (formWoNumber.trim() === "") {
      createError = "WO number required";
      return;
    }
    if (formProductId === "") {
      createError = "Product required";
      return;
    }
    if (formOps.some((o) => o.op_name.trim() === "")) {
      createError = "Every routing op needs a name";
      return;
    }
    creating = true;
    try {
      const body = {
        wo_number: formWoNumber.trim(),
        product_id: formProductId,
        qty_target: formQtyTarget.trim(),
        notes: formNotes.trim() === "" ? null : formNotes.trim(),
        routing_ops: formOps.map((o) => ({
          op_name: o.op_name.trim(),
          est_time_min:
            o.est_time_min.trim() === "" ? null : Number(o.est_time_min.trim()),
          est_cost_huf:
            o.est_cost_huf.trim() === "" ? null : o.est_cost_huf.trim(),
        })),
        idempotency_key: mintIdempotencyKey("create"),
      };
      await createWorkOrder(body);
      // Reset form state + close.
      showCreateForm = false;
      formWoNumber = "";
      formProductId = "";
      formQtyTarget = "1";
      formNotes = "";
      formOps = [{ op_name: "", est_time_min: "", est_cost_huf: "" }];
      await refresh();
    } catch (e) {
      createError = String(e);
    } finally {
      creating = false;
    }
  }

  function setStateFilter(s: WorkOrderState | null): void {
    selectedState = s;
    refresh();
  }

  function actionsForState(s: WorkOrderState): WoAction[] {
    // Mirror of ADR-0062 §2 transition table. The buttons render only
    // for actions whose `from` state is the current state.
    switch (s) {
      case "created":
        return ["release", "cancel"];
      case "released":
        return ["start", "hold", "cancel"];
      case "in_progress":
        return ["complete", "hold", "cancel"];
      case "on_hold":
        return ["resume", "cancel"];
      case "completed":
      case "cancelled":
        return [];
    }
  }

  function productName(id: string): string {
    const p = products.find((p) => p.id === id);
    return p?.name ?? id;
  }

  onMount(refresh);
</script>

<section class="wo-page" aria-labelledby="wo-title">
  <header class="wo-head">
    <h2 id="wo-title">Gyártási rendelések / Work orders</h2>
    <div class="wo-head-actions">
      <button type="button" onclick={refresh}>Frissítés / Refresh</button>
      <button type="button" onclick={() => (showCreateForm = true)}>
        + Új gyártási rendelés / New work order
      </button>
    </div>
  </header>

  <div class="wo-facets" role="tablist" aria-label="State filter">
    {#each STATE_FACETS as f}
      <button
        type="button"
        class="wo-facet"
        class:wo-facet--active={selectedState === f.state}
        onclick={() => setStateFilter(f.state)}
      >
        <span class="wo-facet__hu">{f.hu}</span>
        <span class="wo-facet__en">{f.en}</span>
      </button>
    {/each}
  </div>

  {#if loadState === "loading"}
    <p>Loading…</p>
  {:else if loadState === "error"}
    <p class="wo-error">Error: {loadError}</p>
  {:else if rows.length === 0}
    <p class="wo-empty">No work orders yet.</p>
  {:else}
    <table class="wo-table">
      <thead>
        <tr>
          <th>WO #</th>
          <th>Termék / Product</th>
          <th>Mennyiség / Qty</th>
          <th>Állapot / State</th>
          <th>Létrehozva / Created</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row}
          <tr>
            <td>{row.wo_number}</td>
            <td>{productName(row.product_id)}</td>
            <td>{row.qty_target}</td>
            <td>{row.state}</td>
            <td>{row.created_at}</td>
            <td>
              <button type="button" onclick={() => openDetail(row.wo_id)}>
                Részletek / Open
              </button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if detail !== null}
    <aside class="wo-detail" aria-labelledby="wo-detail-title">
      <header>
        <h3 id="wo-detail-title">
          {detail.work_order.wo_number} — {detail.work_order.state}
        </h3>
        <button type="button" onclick={closeDetail}>Bezár / Close</button>
      </header>
      <dl>
        <dt>Termék / Product</dt>
        <dd>{productName(detail.work_order.product_id)}</dd>
        <dt>Mennyiség / Qty</dt>
        <dd>{detail.work_order.qty_target}</dd>
        <dt>Megjegyzés / Notes</dt>
        <dd>{detail.work_order.notes ?? "—"}</dd>
        {#if detail.work_order.hold_reason !== null}
          <dt>Hold ok / reason</dt>
          <dd>{detail.work_order.hold_reason}</dd>
        {/if}
      </dl>

      {#if actionError !== null}
        <p class="wo-error">Action failed: {actionError}</p>
      {/if}
      {#if warningsToShow.length > 0}
        <ul class="wo-warnings">
          {#each warningsToShow as w}
            <li>{w}</li>
          {/each}
        </ul>
      {/if}

      <div class="wo-actions">
        {#each actionsForState(detail.work_order.state) as a}
          <button type="button" onclick={() => submitTransition(a)}>
            {a}
          </button>
        {/each}
      </div>

      {#if detail.part_marking_required}
        <section class="wo-parts" data-testid="wo-parts">
          <h4>
            Alkatrész-UID / Part UID
            <span class="wo-parts__badge">{detail.customer_type}</span>
          </h4>
          <p class="wo-parts__count">
            {detail.part_marks.length} / {detail.part_units_expected} jelölve / marked
          </p>
          {#if detail.part_marks.length > 0}
            <ul class="wo-parts__chips">
              {#each detail.part_marks as m (m.part_uid)}
                <li
                  class="wo-parts__chip"
                  title={`serial ${m.serial_number} · DM ${m.data_matrix_payload}`}
                >
                  {m.part_uid}
                </li>
              {/each}
            </ul>
          {:else}
            <span class="wo-parts__hint">no parts marked</span>
          {/if}
          {#if detail.work_order.state === "completed" && detail.part_marks.length < detail.part_units_expected}
            <button
              type="button"
              class="wo-parts__mark"
              data-testid="mark-parts-button"
              onclick={() => (showMarkModal = true)}
            >
              Alkatrészek jelölése / Mark parts
            </button>
          {/if}
        </section>
      {/if}

      <section class="wo-routing">
        <h4>Műveletek / Routing operations</h4>
        <table>
          <thead>
            <tr>
              <th>#</th>
              <th>Név / Name</th>
              <th>Idő (perc)</th>
              <th>Költség (HUF)</th>
              <th>Állapot / State</th>
              <th>QA</th>
              <th>Műveletek / Actions</th>
            </tr>
          </thead>
          <tbody>
            {#each detail.routing_ops as op}
              <tr>
                <td>{op.sequence}</td>
                <td>{op.op_name}</td>
                <td>{op.est_time_min ?? "—"}</td>
                <td>{op.est_cost_huf ?? "—"}</td>
                <td>{op.state}</td>
                <td>
                  <span class="wo-qa-chip wo-qa-chip--{qaByOp[op.routing_op_id]?.state ?? 'none'}">
                    {qaChipLabel(qaByOp[op.routing_op_id] ?? null)}
                  </span>
                </td>
                <td>
                  {#if op.state === "active"}
                    <button
                      type="button"
                      class="wo-op-complete"
                      onclick={() => completeOp(op)}
                      disabled={opBusyId === op.routing_op_id}
                    >
                      ✓ Kész / Complete
                    </button>
                  {:else if op.state === "completed"}
                    <span class="wo-op-done">
                      {op.completed_at ? "✓ " + op.completed_at : "✓"}
                    </span>
                  {:else}
                    <span class="wo-op-pending">—</span>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </section>

      <section class="wo-bom">
        <h4>BOM (aktív / active)</h4>
        {#if detail.bom.length === 0}
          <p>No active BOM for this product.</p>
        {:else}
          <table>
            <thead><tr><th>Component</th><th>Qty / unit</th></tr></thead>
            <tbody>
              {#each detail.bom as line}
                <tr>
                  <td>{productName(line.component_id)}</td>
                  <td>{line.qty_per_unit}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      </section>
    </aside>
  {:else if detailLoading}
    <p>Loading detail…</p>
  {:else if detailError !== null}
    <p class="wo-error">Detail load failed: {detailError}</p>
  {/if}

  {#if showMarkModal && detail !== null}
    <MarkPartsModal
      woId={detail.work_order.wo_id}
      expectedUnits={detail.part_units_expected}
      onMarked={onPartsMarked}
      onClose={() => (showMarkModal = false)}
    />
  {/if}

  {#if showCreateForm}
    <div class="wo-modal" role="dialog" aria-labelledby="wo-create-title">
      <div class="wo-modal__body">
        <h3 id="wo-create-title">Új gyártási rendelés / New work order</h3>
        <label>
          WO szám / number
          <input type="text" bind:value={formWoNumber} />
        </label>
        <label>
          Termék / Product
          <select bind:value={formProductId}>
            <option value="">— select —</option>
            {#each products as p}
              <option value={p.id}>{p.name}</option>
            {/each}
          </select>
        </label>
        <label>
          Mennyiség / Qty
          <input type="text" bind:value={formQtyTarget} />
        </label>
        <label>
          Megjegyzés / Notes (optional)
          <textarea bind:value={formNotes}></textarea>
        </label>
        <h4>Műveletek / Routing ops</h4>
        {#each formOps as op, i}
          <div class="wo-op-row">
            <input
              type="text"
              placeholder="Op name"
              bind:value={op.op_name}
            />
            <input
              type="text"
              placeholder="Time (min)"
              bind:value={op.est_time_min}
            />
            <input
              type="text"
              placeholder="Cost (HUF)"
              bind:value={op.est_cost_huf}
            />
            <button type="button" onclick={() => removeOpRow(i)}>×</button>
          </div>
        {/each}
        <button type="button" onclick={addOpRow}>+ add op</button>
        {#if createError !== null}
          <p class="wo-error">{createError}</p>
        {/if}
        <div class="wo-modal__actions">
          <button
            type="button"
            onclick={() => (showCreateForm = false)}
            disabled={creating}
          >
            Mégse / Cancel
          </button>
          <button type="button" onclick={submitCreate} disabled={creating}>
            {creating ? "Saving…" : "Mentés / Save"}
          </button>
        </div>
      </div>
    </div>
  {/if}
</section>

<style>
  /* S232b / PR-228b — dark-theme polish. Tokens only; no hardcoded
     hex. Reference surfaces: PartnerForm.svelte (modal precedent),
     IncomingInvoiceList.svelte (facets + dense table). */

  .wo-page {
    padding: var(--space-4);
    color: var(--color-text-primary);
  }

  .wo-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }

  .wo-head h2 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .wo-head-actions {
    display: flex;
    gap: var(--space-2);
  }

  .wo-head-actions button {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .wo-head-actions button:hover {
    border-color: var(--color-text-muted);
  }

  /* Facet row — mirrors IncomingInvoiceList chip pattern. */
  .wo-facets {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }

  .wo-facet {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .wo-facet:hover {
    color: var(--color-text-strong);
  }

  .wo-facet--active {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .wo-facet__hu {
    display: block;
    font-weight: 600;
  }

  .wo-facet__en {
    display: block;
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  /* Dense list table — mirrors `table.dense` from IncomingInvoiceList. */
  .wo-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-md);
    background: var(--color-surface-sunken);
  }

  .wo-table thead th {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    font-weight: 500;
  }

  .wo-table tbody td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
  }

  .wo-table tbody tr:hover {
    background: var(--color-surface-raised);
  }

  .wo-table tbody button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .wo-table tbody button:hover {
    color: var(--color-text-strong);
  }

  .wo-error {
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm);
  }

  .wo-empty {
    color: var(--color-text-muted);
    padding: var(--space-5);
    border: 1px dashed var(--color-surface-divider);
    background: var(--color-surface-raised);
    text-align: center;
    border-radius: 4px;
  }

  /* Detail aside — sits below the list. */
  .wo-detail {
    margin-top: var(--space-4);
    padding: var(--space-4);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    border-radius: 4px;
  }

  .wo-detail header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }

  .wo-detail header h3 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .wo-detail header button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .wo-detail header button:hover {
    color: var(--color-text-strong);
  }

  .wo-detail dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-1) var(--space-4);
    margin: 0 0 var(--space-3) 0;
  }

  .wo-detail dt {
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }

  .wo-detail dd {
    margin: 0;
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
  }

  .wo-actions {
    margin: var(--space-3) 0;
    display: flex;
    gap: var(--space-2);
  }

  .wo-actions button {
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-4);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .wo-actions button:hover {
    border-color: var(--color-text-muted);
  }

  .wo-warnings {
    background: var(--color-surface-raised);
    border-left: 3px solid var(--color-signal-warning);
    padding: var(--space-2) var(--space-3);
    margin: var(--space-2) 0;
    color: var(--color-text-primary);
    list-style: none;
    font-size: var(--type-size-sm);
  }

  /* S438 — part-UID marking section. */
  .wo-parts {
    margin: var(--space-3) 0;
    padding: var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    background: var(--color-surface-raised);
  }

  .wo-parts h4 {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-strong);
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .wo-parts__badge {
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-signal-positive);
    border: 1px solid var(--color-signal-positive);
    border-radius: 4px;
    padding: 0 var(--space-1);
  }

  .wo-parts__count {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .wo-parts__chips {
    list-style: none;
    margin: 0 0 var(--space-2) 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
  }

  .wo-parts__chip {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    padding: 0 var(--space-2);
    border-radius: 4px;
    color: var(--color-signal-positive);
    background: color-mix(in srgb, var(--color-signal-positive) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--color-signal-positive) 40%, transparent);
  }

  .wo-parts__hint {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    font-style: italic;
  }

  .wo-parts__mark {
    display: block;
    margin-top: var(--space-2);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: 4px;
    padding: var(--space-1) var(--space-4);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .wo-routing,
  .wo-bom {
    margin-top: var(--space-4);
  }

  .wo-routing h4,
  .wo-bom h4 {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .wo-routing table,
  .wo-bom table {
    width: 100%;
    border-collapse: collapse;
    background: var(--color-surface-sunken);
    font-size: var(--type-size-sm);
  }

  .wo-routing th,
  .wo-bom th {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    text-align: left;
    color: var(--color-text-secondary);
    font-weight: 500;
  }

  .wo-routing td,
  .wo-bom td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    text-align: left;
    color: var(--color-text-primary);
  }

  /* S233 / PR-229 — per-op QA chip + Complete button. Same dark-token
     vocabulary as the wo-table chrome above. */
  .wo-qa-chip {
    display: inline-block;
    padding: 2px var(--space-2);
    border-radius: 3px;
    font-size: var(--type-size-xs);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
  }

  .wo-qa-chip--none {
    color: var(--color-text-muted);
  }

  .wo-qa-chip--pending {
    border-color: var(--color-signal-warning);
    color: var(--color-text-strong);
  }

  .wo-qa-chip--passed {
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
    border-color: var(--color-signal-positive);
    font-weight: 500;
  }

  .wo-qa-chip--failed,
  .wo-qa-chip--disposed {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .wo-qa-chip--reworking {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  .wo-op-complete {
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
    border: 1px solid var(--color-signal-positive);
    padding: 2px var(--space-2);
    font-size: var(--type-size-xs);
    border-radius: 4px;
    cursor: pointer;
    font-weight: 500;
  }

  .wo-op-complete:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .wo-op-done {
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
  }

  .wo-op-pending {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }

  /* Modal — mirrors PartnerForm.svelte's dialog frame. */
  .wo-modal {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }

  .wo-modal__body {
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-4) var(--space-5);
    border-radius: 4px;
    max-width: 600px;
    width: 90%;
    max-height: 80vh;
    overflow-y: auto;
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .wo-modal__body h3 {
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .wo-modal__body h4 {
    margin: var(--space-3) 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-strong);
    border-bottom: 1px solid var(--color-surface-divider);
    padding-bottom: var(--space-1);
  }

  .wo-modal__body label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-bottom: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  /* Native form controls — browsers default these to system theme, so
     explicit dark styling is required (see [[spa-dark-theme-default]]). */
  .wo-modal__body label input,
  .wo-modal__body label select,
  .wo-modal__body label textarea {
    display: block;
    width: 100%;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .wo-modal__body label textarea {
    font-family: var(--type-family-body);
    min-height: 4em;
    resize: vertical;
  }

  .wo-op-row {
    display: flex;
    gap: var(--space-2);
    margin-bottom: var(--space-1);
  }

  .wo-op-row input {
    flex: 1;
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .wo-op-row button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    border-radius: 4px;
    cursor: pointer;
  }

  .wo-op-row button:hover {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .wo-modal__body > button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    border-radius: 4px;
    cursor: pointer;
    font-size: var(--type-size-sm);
  }

  .wo-modal__body > button:hover {
    color: var(--color-text-strong);
  }

  .wo-modal__actions {
    margin-top: var(--space-4);
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  /* Cancel = quiet, Save = primary (signal-positive). Order in DOM
     is Cancel then Save, so :last-child = Save. */
  .wo-modal__actions button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2) var(--space-4);
    border-radius: 4px;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .wo-modal__actions button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .wo-modal__actions button:last-child {
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
    border-color: var(--color-signal-positive);
    font-weight: 500;
  }

  .wo-modal__actions button:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
