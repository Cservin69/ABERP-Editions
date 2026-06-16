<script lang="ts">
  // S273 / PR-262 / ADR-0069 — material-side Inventory Balances view.
  //
  // Read-only operator surface: one row per `(tenant, material_grade)`,
  // showing the four ADR-0069 quantities (on_hand / reserved /
  // committed / consumed) plus a server-computed `available_qty =
  // on_hand - reserved - committed`. Rows with `available_qty < 0`
  // render in red (defense-in-depth — the invariant is enforced in
  // `commit_material_in_tx`, so a negative number is a smoking gun
  // for an out-of-tx write).
  //
  // No edit affordance in v1: writes happen via the DEAL saga
  // (`committed +=`) and the future workshop-completion hook
  // (`consumed +=`). Bumping `on_hand_qty` after a material delivery
  // is named-deferred — the future S275+ slice lands an Edit modal +
  // a `MaterialReceipted` audit kind. Until then, an operator who
  // needs to seed a balance for testing can use the DEAL-saga
  // auto-upsert + a direct SQL adjustment (documented in the PR
  // body / runbook).
  //
  // Dark-theme tokens per [[spa-dark-theme-default]].

  import { onMount } from "svelte";

  import {
    listInventoryBalances,
    type InventoryBalance,
  } from "../lib/api";
  import AssignHeatLotModal from "./AssignHeatLotModal.svelte";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let rows = $state<InventoryBalance[]>([]);

  // S432 — the balance row currently being heat-lot-stamped (modal open
  // when non-null). The parent remounts the modal per row by keying on
  // material_grade.
  let assigning = $state<InventoryBalance | null>(null);

  function onAssigned(): void {
    assigning = null;
    void refresh();
  }

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      const res = await listInventoryBalances();
      rows = res.balances;
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function fmt(n: number): string {
    // Two decimals — matches the cost_per_kg display convention in the
    // Material Catalogue view.
    return n.toFixed(2);
  }

  function fmtTs(iso: string): string {
    // Surface as plain ISO; an absolute time is easier to debug than a
    // relative one when an operator is reading two rows back-to-back.
    return iso;
  }
</script>

<section class="ib-page">
  <header class="ib-page__head">
    <h1 class="ib-page__title">
      <span>Anyagkészlet / Inventory balances</span>
      <span class="ib-page__hint">
        Anyagminőség szerint (DEAL-saga csökkenti az `available` mezőt; az
        operátor RECEIPT után frissíti az `on_hand`-et — S275+).
      </span>
      <span class="ib-page__hint">
        Per material grade. The DEAL saga decrements `available` on
        commit; the operator bumps `on_hand` after a delivery (named-
        deferred to S275+).
      </span>
    </h1>
    <div class="ib-page__actions">
      <button
        type="button"
        class="ib-page__refresh"
        onclick={() => void refresh()}
        disabled={loadState === "loading"}
      >
        {loadState === "loading" ? "Frissítés…" : "Frissítés / Refresh"}
      </button>
    </div>
  </header>

  <aside class="ib-page__notice">
    <strong>v1:</strong> a mennyiségek minden DEAL-en stamp-elt
    <code>unit_kind</code>-ot követnek (audit-bejegyzésben látható).
    Pirossal kiemelt sor = <code>available &lt; 0</code> invariáns-sértés.
    <br />
    <strong>v1:</strong> quantities reflect the <code>unit_kind</code>
    stamped on each commit; see the audit ledger entries for forensics.
    Red rows = <code>available &lt; 0</code> invariant breach.
  </aside>

  {#if loadState === "error"}
    <div class="ib-page__error" role="alert">
      <strong>Lista betöltése sikertelen. / Load failed.</strong>
      <p>{errorMessage}</p>
    </div>
  {:else if loadState === "ready" && rows.length === 0}
    <div class="ib-page__empty" role="status">
      Nincs készletadat. Az első DEAL fog `inventory_balances` sort
      felvenni a kérdéses anyagminőséghez (nullával).
      <br />
      No balance rows yet. The first DEAL auto-upserts a row at zero for
      the material grade.
    </div>
  {:else if loadState === "ready"}
    <div class="ib-page__table-wrap">
      <table class="ib-table">
        <thead>
          <tr>
            <th class="ib-table__th ib-table__th--text">Grade</th>
            <th class="ib-table__th ib-table__th--num">On hand</th>
            <th class="ib-table__th ib-table__th--num">Reserved</th>
            <th class="ib-table__th ib-table__th--num" title="qty unit varies — see audit payload for unit_kind">Committed (qty)</th>
            <th class="ib-table__th ib-table__th--num">Consumed</th>
            <th class="ib-table__th ib-table__th--num">Available</th>
            <th class="ib-table__th ib-table__th--text">UoM</th>
            <th class="ib-table__th ib-table__th--text">Heat lot</th>
            <th class="ib-table__th ib-table__th--text">Last updated</th>
            <th class="ib-table__th ib-table__th--text"></th>
          </tr>
        </thead>
        <tbody>
          {#each rows as r (r.material_grade)}
            <tr
              class={r.available_qty < 0
                ? "ib-table__row ib-table__row--breach"
                : "ib-table__row"}
            >
              <td class="ib-table__td ib-table__td--text">
                {r.material_grade}
              </td>
              <td class="ib-table__td ib-table__td--num">
                {fmt(r.on_hand_qty)}
              </td>
              <td class="ib-table__td ib-table__td--num">
                {fmt(r.reserved_qty)}
              </td>
              <td class="ib-table__td ib-table__td--num">
                {fmt(r.committed_qty)}
              </td>
              <td class="ib-table__td ib-table__td--num">
                {fmt(r.consumed_qty)}
              </td>
              <td
                class={r.available_qty < 0
                  ? "ib-table__td ib-table__td--num ib-table__td--breach"
                  : "ib-table__td ib-table__td--num"}
                title={r.available_qty < 0
                  ? "available < 0 — invariant breach"
                  : ""}
              >
                {fmt(r.available_qty)}
              </td>
              <td class="ib-table__td ib-table__td--text">
                {r.unit_of_measure}
              </td>
              <td class="ib-table__td ib-table__td--text">
                {#if r.heat_lot_number}
                  <span
                    class="ib-chip ib-chip--ok"
                    title={r.mill_test_report_url
                      ? `MTR: ${r.mill_test_report_url}`
                      : "no MTR"}
                  >
                    {r.heat_lot_number}
                  </span>
                {:else}
                  <span class="ib-chip__hint">no heat lot</span>
                {/if}
              </td>
              <td
                class="ib-table__td ib-table__td--text ib-table__td--muted"
              >
                {fmtTs(r.last_updated)}
              </td>
              <td class="ib-table__td ib-table__td--text">
                <button
                  type="button"
                  class="ib-row-action"
                  onclick={() => (assigning = r)}
                >
                  Assign heat lot
                </button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</section>

{#if assigning !== null}
  {#key assigning.material_grade}
    <AssignHeatLotModal
      balance={assigning}
      onAssigned={onAssigned}
      onClose={() => (assigning = null)}
    />
  {/key}
{/if}

<style>
  .ib-page {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) 0;
  }
  .ib-page__head {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .ib-page__title {
    font-size: var(--type-size-lg);
    font-weight: 600;
    margin: 0;
    color: var(--color-text-strong);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .ib-page__hint {
    font-size: var(--type-size-sm);
    font-weight: 400;
    color: var(--color-text-muted);
  }
  .ib-page__actions {
    display: flex;
    gap: var(--space-2);
  }
  .ib-page__refresh {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }
  .ib-page__notice {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
    border-radius: 3px;
    font-size: var(--type-size-sm);
    line-height: 1.5;
  }
  .ib-page__notice strong {
    color: var(--color-text-secondary);
  }
  .ib-page__error {
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-negative);
    border-radius: 3px;
    color: var(--color-text-primary);
  }
  .ib-page__error strong {
    color: var(--color-signal-negative);
  }
  .ib-page__empty {
    padding: var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    color: var(--color-text-muted);
    text-align: center;
    line-height: 1.6;
  }
  .ib-page__table-wrap {
    overflow-x: auto;
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    background: var(--color-surface-base);
  }
  .ib-table {
    width: 100%;
    border-collapse: collapse;
    font-variant-numeric: tabular-nums;
  }
  .ib-table__th {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-weight: 600;
    font-size: var(--type-size-sm);
    border-bottom: 1px solid var(--color-surface-divider);
  }
  .ib-table__th--num {
    text-align: right;
  }
  .ib-table__row {
    border-bottom: 1px solid var(--color-surface-divider);
  }
  .ib-table__row:last-child {
    border-bottom: none;
  }
  .ib-table__row--breach {
    background: color-mix(in srgb, var(--color-signal-negative) 6%, transparent);
  }
  .ib-table__td {
    padding: var(--space-2) var(--space-3);
    color: var(--color-text-primary);
    font-size: var(--type-size-sm);
  }
  .ib-table__td--num {
    text-align: right;
  }
  .ib-table__td--text {
    text-align: left;
  }
  .ib-table__td--muted {
    color: var(--color-text-muted);
  }
  .ib-table__td--breach {
    color: var(--color-signal-negative);
    font-weight: 700;
  }
  .ib-chip {
    display: inline-block;
    padding: 1px var(--space-2);
    border-radius: 999px;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    border: 1px solid transparent;
  }
  .ib-chip--ok {
    color: var(--color-signal-positive);
    background: color-mix(in srgb, var(--color-signal-positive) 12%, transparent);
    border-color: color-mix(in srgb, var(--color-signal-positive) 40%, transparent);
  }
  .ib-chip__hint {
    color: var(--color-text-muted);
    font-style: italic;
    font-size: var(--type-size-xs);
  }
  .ib-row-action {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    white-space: nowrap;
  }
  .ib-row-action:hover {
    color: var(--color-text-strong);
  }
</style>
