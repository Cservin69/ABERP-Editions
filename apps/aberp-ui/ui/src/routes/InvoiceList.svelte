<script lang="ts">
  // First dense-table screen — pins the table CSS pattern per
  // ADR-0017. Subsequent screens (invoice detail, audit drill-down,
  // billing summary) inherit this style without re-inventing tokens.
  //
  // Columns:
  //   Invoice id          monospace, primary text; PR-25 makes the id
  //                       a clickable inspector affordance — opens
  //                       the InvoiceDetail modal for that invoice.
  //   Series #            monospace, tabular numbers, right-aligned
  //   Fiscal year         monospace, tabular numbers, right-aligned
  //   State               signal-coloured pill (categorical signal)
  //                       — PR-24 / ADR-0036 §7: chip carries icon +
  //                       label + hover tooltip; eleven labels per
  //                       ADR-0036 §2; per-label affordances come
  //                       from `../lib/labels`. PR-31 / session-35
  //                       appends a small `↘` chain-link badge to
  //                       the right of the chip when the row's
  //                       `has_chain_children` flag is true (the
  //                       invoice is the base of at least one
  //                       storno or amendment chain entry) so an
  //                       inspector can see at a glance which rows
  //                       have a chain history without opening the
  //                       detail modal.
  //   Total (gross, HUF)  monospace, tabular numbers, right-aligned
  //
  // Per ADR-0017 §3 every numeric column is monospace + tabular +
  // right-aligned. Per ADR-0017 §1-2 chrome is quiet, colour means
  // state. Per ADR-0017 §"Adversarial review #4" categorical signal
  // is never carried by colour alone — every chip pairs colour with
  // a glyph + label. Per §4 a freshly-fetched table fades in over
  // 200ms — no spinners, no skeleton shimmers.

  import { onMount } from "svelte";
  import {
    listInvoices,
    type InvoiceListItem,
    type InvoiceState,
  } from "../lib/api";
  import {
    LIFECYCLE_ORDER,
    labelMeta,
    lifecycleIndex,
    type LabelSignal,
  } from "../lib/labels";
  import InvoiceDetail from "./InvoiceDetail.svelte";

  let rows: InvoiceListItem[] = $state([]);
  let loadState: "idle" | "loading" | "loaded" | "error" = $state("idle");
  let errorMessage: string | null = $state(null);

  // Filter dropdown — "All" plus one entry per label. Defaults to
  // "All" so the first paint matches the pre-PR-24 behaviour.
  let filterLabel: "All" | InvoiceState = $state("All");

  // PR-25 / session-29 — selected invoice id drives the detail modal
  // (`null` keeps it closed; a string opens it and triggers the
  // fetch). Per the session-28 handoff lean: modal / in-place over a
  // routed `/invoice/<id>` URL — no SvelteKit dependency added.
  //
  // PR-30 / session-34 — navigation history stack. Each chain-link
  // traversal in `InvoiceDetail.svelte` pushes the base invoice id
  // onto this stack; the modal shows the top of the stack as the
  // current invoice and renders the entries below it as a
  // breadcrumb of `← {id}` back-buttons. Empty stack keeps the
  // modal closed. ESC / backdrop close clears the entire stack
  // (matches the modal-as-single-inspection-context posture from
  // PR-26 / PR-27 / PR-29). Reassignment pattern (build a new
  // array on every mutation) guarantees Svelte 5 reactivity
  // without depending on Array-mutation tracking through the
  // $state proxy.
  let navStack: string[] = $state([]);

  onMount(() => {
    void refresh();
  });

  async function refresh() {
    loadState = "loading";
    errorMessage = null;
    try {
      rows = await listInvoices();
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      errorMessage = err instanceof Error ? err.message : String(err);
    }
  }

  // HUF amount formatter — tabular, no fractional digits because the
  // forint has no sub-unit. Locale `hu-HU` gives space-separated
  // thousands (1 234 567 Ft) which is the Hungarian convention.
  const hufFormatter = new Intl.NumberFormat("hu-HU", {
    style: "currency",
    currency: "HUF",
    minimumFractionDigits: 0,
    maximumFractionDigits: 0,
  });

  function formatHuf(value: number | null): string {
    if (value === null) return "—";
    return hufFormatter.format(value);
  }

  function signalClass(signal: LabelSignal): string {
    return `signal-${signal}`;
  }

  // Filter + lifecycle-natural sort. Per ADR-0036 §3 the operator's
  // mental model walks Unknown → Ready → Pending → PendingNavExists
  // → Submitted → Recovered → Finalized → Rejected → Storno →
  // Amended → Abandoned; that ordering mirrors the audit-ledger
  // ladder in `serve.rs::derive_state`. Within a bucket, secondary
  // sort by invoice id keeps the display stable across refreshes.
  let visibleRows = $derived(
    rows
      .filter((r) => filterLabel === "All" || r.state === filterLabel)
      .slice()
      .sort((a, b) => {
        const dx = lifecycleIndex(a.state) - lifecycleIndex(b.state);
        if (dx !== 0) return dx;
        return a.invoice_id.localeCompare(b.invoice_id);
      }),
  );
</script>

<section class="screen">
  <div class="screen-head">
    <h2>Invoices</h2>
    <div class="actions">
      <label class="filter">
        <span class="filter-label">State</span>
        <select
          bind:value={filterLabel}
          aria-label="Filter invoices by state"
        >
          <option value="All">All</option>
          {#each LIFECYCLE_ORDER as state (state)}
            <option value={state}>{state}</option>
          {/each}
        </select>
      </label>
      <button
        type="button"
        class="quiet-button"
        onclick={() => void refresh()}
        disabled={loadState === "loading"}
      >
        Refresh
      </button>
    </div>
  </div>

  {#if loadState === "error"}
    <p class="error" role="alert">{errorMessage}</p>
  {/if}

  <table class="dense">
    <thead>
      <tr>
        <th scope="col" class="col-id">Invoice id</th>
        <th scope="col" class="col-num">Series #</th>
        <th scope="col" class="col-num">Fiscal year</th>
        <th scope="col" class="col-state">State</th>
        <th scope="col" class="col-num">Total (gross)</th>
      </tr>
    </thead>
    <tbody>
      {#if loadState === "loaded" && visibleRows.length === 0}
        <tr class="empty">
          <td colspan="5">
            {#if rows.length === 0}
              No invoices on this tenant yet. Issue one with
              <code>aberp issue-invoice</code> and reload.
            {:else}
              No invoices match the filter
              <code>{filterLabel}</code>.
            {/if}
          </td>
        </tr>
      {/if}
      {#each visibleRows as row (row.invoice_id)}
        {@const meta = labelMeta(row.state)}
        <tr>
          <td class="col-id mono">
            <button
              type="button"
              class="id-link"
              onclick={() => (navStack = [row.invoice_id])}
              aria-label={`Open detail for invoice ${row.invoice_id}`}
            >
              {row.invoice_id}
            </button>
          </td>
          <td class="col-num mono">{row.sequence_number}</td>
          <td class="col-num mono">{row.fiscal_year}</td>
          <td class="col-state">
            <span
              class="state-pill {signalClass(meta.signal)}"
              title={meta.tooltip}
            >
              <span class="state-icon" aria-hidden="true">{meta.icon}</span>
              <span class="state-text">{row.state}</span>
            </span>
            {#if row.has_chain_children}
              <span
                class="chain-badge"
                aria-label="This invoice is the base of a storno or amendment chain"
                title="This invoice is the base of a storno or amendment chain — open the row to inspect."
              >↘</span>
            {/if}
          </td>
          <td class="col-num mono">{formatHuf(row.total_gross)}</td>
        </tr>
      {/each}
    </tbody>
  </table>

  <InvoiceDetail
    invoiceId={navStack.length > 0 ? navStack[navStack.length - 1] : null}
    ancestors={navStack.slice(0, -1)}
    onClose={() => (navStack = [])}
    onNavigate={(baseId) => (navStack = [...navStack, baseId])}
    onJumpBack={(index) => (navStack = navStack.slice(0, index + 1))}
  />
</section>

<style>
  .screen {
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .screen-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    margin-bottom: var(--space-3);
  }

  h2 {
    margin: 0;
    font-size: var(--type-size-xl);
    font-weight: 500;
    color: var(--color-text-strong);
    letter-spacing: 0.02em;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-3);
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

  .filter select {
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

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    transition: color var(--motion-fade-in);
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quiet-button:disabled {
    opacity: 0.5;
    cursor: progress;
  }

  .error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: var(--space-2) 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  /* Dense table — the load-bearing CSS of ADR-0017. */
  table.dense {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-md);
    background: var(--color-surface-sunken);
  }

  table.dense thead th {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    font-weight: 500;
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  table.dense tbody td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
  }

  table.dense tbody tr:hover {
    background: var(--color-surface-raised);
  }

  /* Tabular figures for every numeric column — ADR-0017 §3. */
  td.mono,
  .mono {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
  }

  .col-num {
    text-align: right;
  }

  .col-id {
    width: 30ch;
  }

  /* PR-25 — invoice-id cell is a clickable inspector affordance.
   * Reset the native button chrome; visually it reads as a quiet
   * link so the dense-table aesthetic per ADR-0017 is preserved. */
  .id-link {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: var(--color-text-primary);
    text-align: left;
    cursor: pointer;
  }

  .id-link:hover,
  .id-link:focus-visible {
    color: var(--color-text-strong);
    text-decoration: underline;
    text-decoration-color: var(--color-text-muted);
    text-underline-offset: 2px;
  }

  .id-link:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  /* Widened from 14ch to 22ch — PendingNavExists (16 chars) plus
   * icon + gap is the longest chip; the column must fit without
   * wrapping. PR-31 / session-35 — the chain-link badge appends a
   * single glyph (`↘`) with a small left margin to the cell; the
   * 22ch floor still fits the badge alongside the longest chip
   * because PendingNavExists is the only state that pairs with a
   * chain-children flag rarely (the typical chain-base state is
   * Storno or Amended, both shorter). */
  .col-state {
    width: 22ch;
  }

  /* PR-31 / session-35 — chain-link badge next to the state chip.
   * Quiet aesthetic per ADR-0017 §1-2: muted text, no border,
   * cursor-help to mirror the state pill's hover-tooltip
   * convention. Categorical signal is the glyph itself per
   * ADR-0017 §"Adversarial review #4" — colour is not the
   * load-bearing signal. */
  .chain-badge {
    margin-left: var(--space-1);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    color: var(--color-text-muted);
    cursor: help;
  }

  .state-pill {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 0 var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    line-height: 1.6;
    letter-spacing: 0.04em;
    border: 1px solid var(--color-surface-divider);
    border-radius: 2px;
    background: var(--color-surface-base);
    color: var(--color-text-secondary);
    cursor: help;
  }

  .state-icon {
    /* Glyphs render slightly sharper in the body face than the
     * mono face at small sizes; the chip's overall mono character
     * is preserved by the label text + tabular alignment. */
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    line-height: 1;
  }

  /* Categorical signal colours — only state cells carry colour. */
  .state-pill.signal-positive {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }
  .state-pill.signal-negative {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .state-pill.signal-warning {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  /* PR-24 — reserved violet per ADR-0017 §5. Surfaces ABERP↔NAV
   * record disagreement at the label level; `PendingNavExists` is
   * the exact operator-visible case. */
  .state-pill.signal-divergence {
    color: var(--color-signal-divergence);
    border-color: var(--color-signal-divergence);
  }
  .state-pill.signal-muted {
    color: var(--color-text-muted);
    border-color: var(--color-surface-divider);
  }

  .empty td {
    color: var(--color-text-muted);
    font-style: italic;
    text-align: center;
    padding: var(--space-5) var(--space-3);
  }

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }
</style>
