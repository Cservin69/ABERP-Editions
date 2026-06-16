<script lang="ts">
  // S432 — Material traceability. The operator-facing chain-of-custody
  // report: look up a material id OR a heat lot and see the balance row,
  // the quotes + work-orders that touched it, and an invoices placeholder
  // (not tracked yet). Operational area, no dashboard tile.
  //
  // Dark-theme tokens per [[spa-dark-theme-default]].

  import {
    materialTraceability,
    partTraceability,
    type MaterialTraceReport,
    type PartTraceReport,
  } from "../lib/api";

  type Mode = "material_id" | "heat_lot";
  type LoadState = "idle" | "loading" | "ready" | "error";

  let mode = $state<Mode>("material_id");
  let needle = $state("");
  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let report = $state<MaterialTraceReport | null>(null);

  let canTrace = $derived(needle.trim().length > 0 && loadState !== "loading");

  // S438 — Part UID Lookup (forward: part_uid → chain; reverse: customer_id →
  // all part UIDs shipped). Its own search state, sharing the page's styles.
  type PartMode = "part_uid" | "customer";
  let partMode = $state<PartMode>("part_uid");
  let partNeedle = $state("");
  let partLoadState = $state<LoadState>("idle");
  let partError = $state<string | null>(null);
  let partReport = $state<PartTraceReport | null>(null);

  let canTracePart = $derived(
    partNeedle.trim().length > 0 && partLoadState !== "loading",
  );

  async function tracePart(): Promise<void> {
    const value = partNeedle.trim();
    if (value.length === 0) return;
    partLoadState = "loading";
    partError = null;
    try {
      const params =
        partMode === "part_uid" ? { partUid: value } : { customerId: value };
      partReport = await partTraceability(params);
      partLoadState = "ready";
    } catch (e) {
      partError = e instanceof Error ? e.message : String(e);
      partLoadState = "error";
    }
  }

  function onPartSubmit(event: Event): void {
    event.preventDefault();
    void tracePart();
  }

  function dash(s: string | null | undefined): string {
    return s && s.trim().length > 0 ? s : "—";
  }

  async function trace(): Promise<void> {
    const value = needle.trim();
    if (value.length === 0) return;
    loadState = "loading";
    errorMessage = null;
    try {
      const params =
        mode === "material_id" ? { materialId: value } : { heatLot: value };
      report = await materialTraceability(params);
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function onSubmit(event: Event): void {
    event.preventDefault();
    void trace();
  }
</script>

<section class="mt-page">
  <header class="mt-page__head">
    <h1 class="mt-page__title">
      <span>Anyagkövethetőség / Material traceability</span>
      <span class="mt-page__hint">
        Anyagazonosító vagy hőkezelési tétel alapján — származási lánc
        (ajánlatok, munkalapok, számlák).
      </span>
      <span class="mt-page__hint">
        Look up by material id or heat lot — the chain of custody
        (quotes, work orders, invoices).
      </span>
    </h1>
  </header>

  <form class="mt-search" onsubmit={onSubmit}>
    <div class="mt-search__modes" role="radiogroup" aria-label="Lookup mode">
      <label class="mt-radio">
        <input
          type="radio"
          name="mt-mode"
          value="material_id"
          checked={mode === "material_id"}
          onchange={() => (mode = "material_id")}
        />
        <span>By material id</span>
      </label>
      <label class="mt-radio">
        <input
          type="radio"
          name="mt-mode"
          value="heat_lot"
          checked={mode === "heat_lot"}
          onchange={() => (mode = "heat_lot")}
        />
        <span>By heat lot</span>
      </label>
    </div>
    <input
      class="mt-search__input"
      type="text"
      bind:value={needle}
      data-testid="trace-input"
      placeholder={mode === "material_id" ? "material id / grade" : "heat lot"}
      autocomplete="off"
    />
    <button type="submit" class="mt-search__btn" disabled={!canTrace}>
      {loadState === "loading" ? "Trace…" : "Trace / Lekérdez"}
    </button>
  </form>

  {#if loadState === "error"}
    <div class="mt-error" role="alert">
      <strong>Lekérdezés sikertelen. / Trace failed.</strong>
      <p>{errorMessage}</p>
    </div>
  {:else if loadState === "ready" && report !== null}
    <div class="mt-report">
      <section class="mt-block">
        <h2 class="mt-block__title">Material / Anyag</h2>
        {#if report.material !== null}
          <dl class="mt-kv">
            <dt>Grade</dt>
            <dd>{dash(report.material.material_grade)}</dd>
            <dt>Heat lot</dt>
            <dd>{dash(report.material.heat_lot_number)}</dd>
            <dt>MTR URL</dt>
            <dd class="mt-kv__mono">{dash(report.material.mill_test_report_url)}</dd>
            <dt>Assigned by</dt>
            <dd>{dash(report.material.heat_assigned_by_operator)}</dd>
            <dt>Assigned at</dt>
            <dd>{dash(report.material.heat_assigned_at_utc)}</dd>
          </dl>
        {:else}
          <p class="mt-empty">
            Nincs készletsor ehhez a kérdéshez. / No balance row for
            <strong>{report.query_value}</strong>.
          </p>
        {/if}
      </section>

      <section class="mt-block">
        <h2 class="mt-block__title">Quotes / Ajánlatok</h2>
        {#if report.quotes.length === 0}
          <p class="mt-empty">No quotes.</p>
        {:else}
          <table class="mt-table">
            <thead>
              <tr>
                <th class="mt-table__th">Quote id</th>
                <th class="mt-table__th">State</th>
              </tr>
            </thead>
            <tbody>
              {#each report.quotes as q (q.quote_id)}
                <tr class="mt-table__row">
                  <td class="mt-table__td mt-table__td--mono">{q.quote_id}</td>
                  <td class="mt-table__td">{q.state}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      </section>

      <section class="mt-block">
        <h2 class="mt-block__title">Work orders / Munkalapok</h2>
        {#if report.work_orders.length === 0}
          <p class="mt-empty">No work orders.</p>
        {:else}
          <table class="mt-table">
            <thead>
              <tr>
                <th class="mt-table__th">WO id</th>
                <th class="mt-table__th">WO number</th>
                <th class="mt-table__th">State</th>
              </tr>
            </thead>
            <tbody>
              {#each report.work_orders as w (w.wo_id)}
                <tr class="mt-table__row">
                  <td class="mt-table__td mt-table__td--mono">{w.wo_id}</td>
                  <td class="mt-table__td">{w.wo_number}</td>
                  <td class="mt-table__td">{w.state}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      </section>

      <section class="mt-block">
        <h2 class="mt-block__title">Invoices / Számlák</h2>
        {#if report.invoices.length === 0}
          <p class="mt-empty">{report.invoices_note}</p>
        {:else}
          <ul class="mt-list">
            {#each report.invoices as inv (inv)}
              <li class="mt-list__item mt-table__td--mono">{inv}</li>
            {/each}
          </ul>
        {/if}
      </section>
    </div>
  {/if}

  <!-- S438 — Part UID Lookup (forward + reverse), under the same tab. -->
  <header class="mt-subhead">
    <h2 class="mt-subhead__title">Alkatrész UID keresés / Part UID Lookup</h2>
    <span class="mt-page__hint">
      Part UID alapján a teljes lánc, vagy ügyfél alapján minden kiszállított
      alkatrész. / By part UID the full chain, or by customer every shipped
      part UID.
    </span>
  </header>

  <form class="mt-search" onsubmit={onPartSubmit}>
    <div class="mt-search__modes" role="radiogroup" aria-label="Part lookup mode">
      <label class="mt-radio">
        <input
          type="radio"
          name="mt-part-mode"
          value="part_uid"
          checked={partMode === "part_uid"}
          onchange={() => (partMode = "part_uid")}
        />
        <span>By part UID</span>
      </label>
      <label class="mt-radio">
        <input
          type="radio"
          name="mt-part-mode"
          value="customer"
          checked={partMode === "customer"}
          onchange={() => (partMode = "customer")}
        />
        <span>By customer</span>
      </label>
    </div>
    <input
      class="mt-search__input"
      type="text"
      bind:value={partNeedle}
      data-testid="part-trace-input"
      placeholder={partMode === "part_uid" ? "dp-… part UID" : "customer partner id"}
      autocomplete="off"
    />
    <button type="submit" class="mt-search__btn" disabled={!canTracePart}>
      {partLoadState === "loading" ? "Trace…" : "Trace / Lekérdez"}
    </button>
  </form>

  {#if partLoadState === "error"}
    <div class="mt-error" role="alert">
      <strong>Lekérdezés sikertelen. / Trace failed.</strong>
      <p>{partError}</p>
    </div>
  {:else if partLoadState === "ready" && partReport !== null}
    <section class="mt-block">
      <h2 class="mt-block__title">Parts / Alkatrészek</h2>
      {#if partReport.parts.length === 0}
        <p class="mt-empty">
          Nincs találat. / No part found for
          <strong>{partReport.query_value}</strong>.
        </p>
      {:else}
        <table class="mt-table">
          <thead>
            <tr>
              <th class="mt-table__th">Part UID</th>
              <th class="mt-table__th">Serial</th>
              <th class="mt-table__th">Heat lot</th>
              <th class="mt-table__th">WO</th>
              <th class="mt-table__th">Quote</th>
              <th class="mt-table__th">Customer</th>
            </tr>
          </thead>
          <tbody>
            {#each partReport.parts as p (p.part_uid)}
              <tr class="mt-table__row">
                <td class="mt-table__td mt-table__td--mono">{p.part_uid}</td>
                <td class="mt-table__td">{p.serial_number}</td>
                <td class="mt-table__td mt-table__td--mono">{dash(p.heat_lot_reference)}</td>
                <td class="mt-table__td mt-table__td--mono">{p.wo_number}</td>
                <td class="mt-table__td mt-table__td--mono">{dash(p.source_quote_id)}</td>
                <td class="mt-table__td">{dash(p.customer_name)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>
  {/if}
</section>

<style>
  .mt-page {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) 0;
  }
  .mt-page__head {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .mt-page__title {
    font-size: var(--type-size-lg);
    font-weight: 600;
    margin: 0;
    color: var(--color-text-strong);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .mt-page__hint {
    font-size: var(--type-size-sm);
    font-weight: 400;
    color: var(--color-text-muted);
  }
  /* S438 — Part UID Lookup subheader divider. */
  .mt-subhead {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-top: var(--space-4);
    padding-top: var(--space-3);
    border-top: 1px solid var(--color-surface-divider);
  }
  .mt-subhead__title {
    margin: 0;
    font-size: var(--type-size-md, var(--type-size-sm));
    font-weight: 600;
    color: var(--color-text-strong);
  }
  .mt-search {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-wrap: wrap;
    padding: var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    border-radius: 3px;
  }
  .mt-search__modes {
    display: flex;
    gap: var(--space-3);
  }
  .mt-radio {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }
  .mt-search__input {
    flex: 1 1 220px;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }
  .mt-search__btn {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-secondary);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }
  .mt-search__btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .mt-error {
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-negative);
    border-radius: 3px;
    color: var(--color-text-primary);
  }
  .mt-error strong {
    color: var(--color-signal-negative);
  }
  .mt-report {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .mt-block {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .mt-block__title {
    margin: 0;
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .mt-empty {
    margin: 0;
    padding: var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }
  .mt-kv {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0;
    padding: var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    background: var(--color-surface-base);
  }
  .mt-kv dt {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }
  .mt-kv dd {
    margin: 0;
    color: var(--color-text-primary);
    font-size: var(--type-size-sm);
  }
  .mt-kv__mono {
    font-family: var(--type-family-mono);
    word-break: break-all;
  }
  .mt-table {
    width: 100%;
    border-collapse: collapse;
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    background: var(--color-surface-base);
    font-variant-numeric: tabular-nums;
  }
  .mt-table__th {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-weight: 600;
    font-size: var(--type-size-sm);
    border-bottom: 1px solid var(--color-surface-divider);
  }
  .mt-table__row {
    border-bottom: 1px solid var(--color-surface-divider);
  }
  .mt-table__row:last-child {
    border-bottom: none;
  }
  .mt-table__td {
    padding: var(--space-2) var(--space-3);
    color: var(--color-text-primary);
    font-size: var(--type-size-sm);
  }
  .mt-table__td--mono {
    font-family: var(--type-family-mono);
  }
  .mt-list {
    margin: 0;
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    background: var(--color-surface-base);
  }
  .mt-list__item {
    color: var(--color-text-primary);
    font-size: var(--type-size-sm);
  }
</style>
