<script lang="ts">
  // S225 / PR-221 — financial-statistics dashboard. Read-only view over
  // the backend's `/api/reports/financial` aggregator (outgoing native
  // invoices + restored NAV-mirror rows + AP-side incoming invoices +
  // audit-ledger-derived state).
  //
  // Period selector defaults to current month (matches the HU monthly
  // bevallás cadence). Date basis defaults to `teljesites` (delivery
  // date — the regulatory anchor for VAT-month assignment per
  // [[aberp-invoice-dates]]).
  //
  // The page is intentionally a single big read on mount + on any
  // period / basis change. There are no writes; no audit-ledger emits;
  // no mutations to local state beyond the report blob itself. Failure
  // surfaces inline with a Retry button (CLAUDE.md rule 12 — fail
  // loud).

  import { onMount } from "svelte";
  import {
    getFinancialReport,
    type FinancialReport,
  } from "../lib/api";
  import {
    buildPeriodOptions,
    formatHuf,
    formatMinor,
    formatPctChange,
    formatVatRate,
    isAggregateEmpty,
    type DateBasis,
  } from "../lib/statistics";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState: LoadState = $state("idle");
  let errorMessage = $state<string | null>(null);
  let report: FinancialReport | null = $state(null);

  // Default to current month (empty string → backend chooses current
  // month). Operator can change via the dropdown.
  let periodOptions = $state(buildPeriodOptions(new Date()));
  let selectedPeriod = $state<string>(periodOptions[0]?.wire ?? "");
  let dateBasis: DateBasis = $state("teljesites");

  onMount(() => {
    void load();
  });

  async function load() {
    loadState = "loading";
    errorMessage = null;
    try {
      const r = await getFinancialReport(selectedPeriod, dateBasis);
      report = r;
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function onPeriodChange(e: Event) {
    const target = e.target as HTMLSelectElement;
    selectedPeriod = target.value;
    void load();
  }

  function setDateBasis(next: DateBasis) {
    if (next === dateBasis) return;
    dateBasis = next;
    void load();
  }
</script>

<section class="stats" aria-labelledby="stats-title">
  <header class="stats__head">
    <h2 id="stats-title">Financial dashboard / Pénzügyi áttekintő</h2>
    <div class="stats__controls">
      <div class="stats__basis" role="tablist" aria-label="Date basis">
        <button
          type="button"
          role="tab"
          aria-selected={dateBasis === "teljesites"}
          class="stats__basis-btn"
          class:active={dateBasis === "teljesites"}
          onclick={() => setDateBasis("teljesites")}
        >
          Teljesítés
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={dateBasis === "issued"}
          class="stats__basis-btn"
          class:active={dateBasis === "issued"}
          onclick={() => setDateBasis("issued")}
        >
          Kiállítás
        </button>
      </div>
      <label class="stats__period">
        Period
        <select
          aria-label="Period"
          value={selectedPeriod}
          onchange={onPeriodChange}
        >
          {#each periodOptions as opt (opt.wire)}
            <option value={opt.wire}>{opt.label}</option>
          {/each}
        </select>
      </label>
    </div>
  </header>

  {#if loadState === "loading"}
    <p class="stats__loading">Loading aggregates…</p>
  {:else if loadState === "error"}
    <div class="stats__error" role="alert">
      <strong>Could not load report.</strong>
      <p>{errorMessage ?? "Unknown error"}</p>
      <button type="button" onclick={() => void load()}>Retry</button>
    </div>
  {:else if loadState === "ready" && report !== null}
    {@const r = report}
    <p class="stats__meta">
      <span><strong>Period:</strong> {r.period.label}</span>
      <span><strong>Date basis:</strong> {r.period.date_basis}</span>
      <span><strong>Today:</strong> {r.period.today}</span>
    </p>

    <!-- Row 1: revenue / expenses / gross profit / VAT-to-pay -->
    <section class="stats__cards" aria-label="Headline figures">
      <article class="stats__card">
        <h3>Revenue / Bevétel</h3>
        {#if isAggregateEmpty(r.revenue)}
          <p class="stats__empty">— no data for this period —</p>
        {:else}
          <p class="stats__row">
            <span>HUF</span><span class="num">{formatHuf(r.revenue.huf.gross_minor)}</span>
            <span class="muted">({r.revenue.huf.count})</span>
          </p>
          <p class="stats__row">
            <span>EUR</span><span class="num">{formatMinor(r.revenue.eur.gross_minor, "EUR")}</span>
            <span class="muted">({r.revenue.eur.count})</span>
          </p>
        {/if}
        {#if r.deltas.yoy !== null}
          <p class="stats__delta">
            <span class="stats__delta-label">YoY</span>
            HUF <span class="delta" class:up={(r.deltas.yoy.revenue_pct_huf ?? 0) > 0} class:down={(r.deltas.yoy.revenue_pct_huf ?? 0) < 0}>{formatPctChange(r.deltas.yoy.revenue_pct_huf)}</span>
            · EUR <span class="delta" class:up={(r.deltas.yoy.revenue_pct_eur ?? 0) > 0} class:down={(r.deltas.yoy.revenue_pct_eur ?? 0) < 0}>{formatPctChange(r.deltas.yoy.revenue_pct_eur)}</span>
          </p>
        {/if}
        {#if r.deltas.mom !== null}
          <p class="stats__delta">
            <span class="stats__delta-label">MoM</span>
            HUF <span class="delta" class:up={(r.deltas.mom.revenue_pct_huf ?? 0) > 0} class:down={(r.deltas.mom.revenue_pct_huf ?? 0) < 0}>{formatPctChange(r.deltas.mom.revenue_pct_huf)}</span>
            · EUR <span class="delta" class:up={(r.deltas.mom.revenue_pct_eur ?? 0) > 0} class:down={(r.deltas.mom.revenue_pct_eur ?? 0) < 0}>{formatPctChange(r.deltas.mom.revenue_pct_eur)}</span>
          </p>
        {/if}
      </article>

      <article class="stats__card">
        <h3>Expenses / Kiadás</h3>
        {#if isAggregateEmpty(r.expenses)}
          <p class="stats__empty">— no data for this period —</p>
        {:else}
          <p class="stats__row">
            <span>HUF</span><span class="num">{formatHuf(r.expenses.huf.gross_minor)}</span>
            <span class="muted">({r.expenses.huf.count})</span>
          </p>
          <p class="stats__row">
            <span>EUR</span><span class="num">{formatMinor(r.expenses.eur.gross_minor, "EUR")}</span>
            <span class="muted">({r.expenses.eur.count})</span>
          </p>
        {/if}
        {#if r.deltas.yoy !== null}
          <p class="stats__delta">
            <span class="stats__delta-label">YoY</span>
            HUF <span class="delta" class:up={(r.deltas.yoy.expenses_pct_huf ?? 0) > 0} class:down={(r.deltas.yoy.expenses_pct_huf ?? 0) < 0}>{formatPctChange(r.deltas.yoy.expenses_pct_huf)}</span>
            · EUR <span class="delta" class:up={(r.deltas.yoy.expenses_pct_eur ?? 0) > 0} class:down={(r.deltas.yoy.expenses_pct_eur ?? 0) < 0}>{formatPctChange(r.deltas.yoy.expenses_pct_eur)}</span>
          </p>
        {/if}
      </article>

      <article class="stats__card">
        <h3>Gross profit / Bruttó eredmény</h3>
        <p class="stats__row">
          <span>HUF</span><span class="num">{formatHuf(r.gross_profit.huf_minor)}</span>
        </p>
        <p class="stats__row">
          <span>EUR</span><span class="num">{formatMinor(r.gross_profit.eur_minor, "EUR")}</span>
        </p>
      </article>

      <article class="stats__card">
        <h3>VAT to pay / ÁFA fizetendő</h3>
        <p class="stats__row">
          <span>HUF</span><span class="num">{formatHuf(r.vat_to_pay.huf_minor)}</span>
        </p>
        <p class="stats__row">
          <span>EUR</span><span class="num">{formatMinor(r.vat_to_pay.eur_minor, "EUR")}</span>
        </p>
        <p class="stats__detail">
          Collected HUF {formatHuf(r.vat_collected.huf.vat_minor)} · EUR {formatMinor(r.vat_collected.eur.vat_minor, "EUR")}
        </p>
        <p class="stats__detail">
          Paid HUF {formatHuf(r.vat_paid.huf.vat_minor)} · EUR {formatMinor(r.vat_paid.eur.vat_minor, "EUR")}
        </p>
      </article>
    </section>

    <!-- Row 2: AR, AP, DSO, cashflow -->
    <section class="stats__cards" aria-label="Working-capital metrics">
      <article class="stats__card">
        <h3>Receivables (AR) / Vevőkövetelés</h3>
        <p class="stats__row">
          <span>HUF</span><span class="num">{formatHuf(r.receivables.huf.gross_minor)}</span>
          <span class="muted">({r.receivables.huf.count})</span>
        </p>
        <p class="stats__row">
          <span>EUR</span><span class="num">{formatMinor(r.receivables.eur.gross_minor, "EUR")}</span>
          <span class="muted">({r.receivables.eur.count})</span>
        </p>
      </article>

      <article class="stats__card">
        <h3>Payables (AP) / Szállítói tartozás</h3>
        <p class="stats__row">
          <span>HUF</span><span class="num">{formatHuf(r.payables.huf.gross_minor)}</span>
          <span class="muted">({r.payables.huf.count})</span>
        </p>
        <p class="stats__row">
          <span>EUR</span><span class="num">{formatMinor(r.payables.eur.gross_minor, "EUR")}</span>
          <span class="muted">({r.payables.eur.count})</span>
        </p>
      </article>

      <article class="stats__card">
        <h3>DSO (avg days to pay)</h3>
        <p class="stats__row">
          <span>HUF</span>
          <span class="num">
            {r.dso_days.huf_days === null ? "—" : `${r.dso_days.huf_days.toFixed(1)}d`}
          </span>
          <span class="muted">(n={r.dso_days.huf_sample_size})</span>
        </p>
        <p class="stats__row">
          <span>EUR</span>
          <span class="num">
            {r.dso_days.eur_days === null ? "—" : `${r.dso_days.eur_days.toFixed(1)}d`}
          </span>
          <span class="muted">(n={r.dso_days.eur_sample_size})</span>
        </p>
      </article>

      <article class="stats__card">
        <h3>Cash-flow forward (gross of receivables due)</h3>
        <p class="stats__row">
          <span>Next 30d</span>
          <span class="num">
            HUF {formatHuf(r.cashflow_forward.next_30.huf_minor)} · EUR {formatMinor(r.cashflow_forward.next_30.eur_minor, "EUR")}
          </span>
        </p>
        <p class="stats__row">
          <span>Next 60d</span>
          <span class="num">
            HUF {formatHuf(r.cashflow_forward.next_60.huf_minor)} · EUR {formatMinor(r.cashflow_forward.next_60.eur_minor, "EUR")}
          </span>
        </p>
        <p class="stats__row">
          <span>Next 90d</span>
          <span class="num">
            HUF {formatHuf(r.cashflow_forward.next_90.huf_minor)} · EUR {formatMinor(r.cashflow_forward.next_90.eur_minor, "EUR")}
          </span>
        </p>
      </article>
    </section>

    <!-- Row 3: VAT-by-rate breakdown -->
    <section class="stats__breakdown" aria-label="VAT-by-rate breakdown">
      <h3>VAT breakdown (outgoing native invoices)</h3>
      {#if r.vat_breakdown_outgoing.length === 0}
        <p class="stats__empty">— no taxable line items in this period —</p>
      {:else}
        <table class="stats__table">
          <thead>
            <tr>
              <th>Rate</th>
              <th>Currency</th>
              <th class="num">Net</th>
              <th class="num">VAT</th>
            </tr>
          </thead>
          <tbody>
            {#each r.vat_breakdown_outgoing as v (`${v.currency}-${v.rate_basis_points}`)}
              <tr>
                <td>{formatVatRate(v.rate_basis_points)}</td>
                <td>{v.currency}</td>
                <td class="num">
                  {v.currency === "EUR"
                    ? formatMinor(v.net_minor, "EUR")
                    : formatHuf(v.net_minor)}
                </td>
                <td class="num">
                  {v.currency === "EUR"
                    ? formatMinor(v.vat_minor, "EUR")
                    : formatHuf(v.vat_minor)}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>

    <!-- Row 4: Top-N -->
    <section class="stats__top" aria-label="Top customers and vendors">
      <article>
        <h3>Top customers (by gross)</h3>
        {#if r.top_customers.length === 0}
          <p class="stats__empty">— no customer-labelled invoices in this period —</p>
        {:else}
          <ol>
            {#each r.top_customers as t (`${t.label}-${t.currency}`)}
              <li>
                {t.label} —
                <strong>
                  {t.currency === "EUR" ? formatMinor(t.gross_minor, "EUR") : formatHuf(t.gross_minor)}
                </strong>
                <span class="muted">({t.count})</span>
              </li>
            {/each}
          </ol>
        {/if}
      </article>
      <article>
        <h3>Top vendors (by spend)</h3>
        {#if r.top_vendors.length === 0}
          <p class="stats__empty">— no vendor activity in this period —</p>
        {:else}
          <ol>
            {#each r.top_vendors as t (`${t.label}-${t.currency}`)}
              <li>
                {t.label} —
                <strong>
                  {t.currency === "EUR" ? formatMinor(t.gross_minor, "EUR") : formatHuf(t.gross_minor)}
                </strong>
                <span class="muted">({t.count})</span>
              </li>
            {/each}
          </ol>
        {/if}
      </article>
    </section>

    <!-- Row 5: Hygiene flags -->
    <section class="stats__hygiene" aria-label="Hygiene flags">
      <h3>Hygiene</h3>
      <ul>
        <li class:flag-nonzero={r.hygiene.outgoing_pending_count > 0}>Pending drafts (outgoing): <strong>{r.hygiene.outgoing_pending_count}</strong></li>
        <li class:flag-nonzero={r.hygiene.outgoing_rejected_count > 0}>Rejected by NAV: <strong>{r.hygiene.outgoing_rejected_count}</strong></li>
        <li class:flag-nonzero={r.hygiene.outgoing_abandoned_count > 0}>Abandoned: <strong>{r.hygiene.outgoing_abandoned_count}</strong></li>
        <li class:flag-nonzero={r.hygiene.restored_no_partner_count > 0}>Restored rows with no partner link: <strong>{r.hygiene.restored_no_partner_count}</strong></li>
        <li class:flag-nonzero={r.hygiene.outstanding_past_deadline_count > 0}>Outstanding receivables past deadline: <strong>{r.hygiene.outstanding_past_deadline_count}</strong></li>
        <li class:flag-nonzero={r.hygiene.payable_past_deadline_count > 0}>Outstanding payables past deadline: <strong>{r.hygiene.payable_past_deadline_count}</strong></li>
        <li class:flag-nonzero={r.hygiene.storno_chain_count > 0}>Storno chain entries in period: <strong>{r.hygiene.storno_chain_count}</strong></li>
        <li class:flag-nonzero={r.hygiene.modification_chain_count > 0}>Modification chain entries in period: <strong>{r.hygiene.modification_chain_count}</strong></li>
      </ul>
    </section>

    <!-- Annual running total -->
    <section class="stats__annual" aria-label="Year-to-date running total">
      <h3>Year-to-date revenue ({r.annual_running.year})</h3>
      <p class="stats__row">
        <span>HUF</span><span class="num">{formatHuf(r.annual_running.revenue.huf.gross_minor)}</span>
        <span class="muted">({r.annual_running.revenue.huf.count})</span>
      </p>
      <p class="stats__row">
        <span>EUR</span><span class="num">{formatMinor(r.annual_running.revenue.eur.gross_minor, "EUR")}</span>
        <span class="muted">({r.annual_running.revenue.eur.count})</span>
      </p>
    </section>

    <details class="stats__deferred">
      <summary>Deferred to a later release</summary>
      <ul>
        {#each r.deferred_notes as note (note)}
          <li>{note}</li>
        {/each}
      </ul>
    </details>
  {/if}
</section>

<style>
  /* S226 / PR-222 — dark-theme colour polish. Every colour resolves to a
   * tokens.css variable (ADR-0017); the prior revision referenced
   * undefined names (--color-surface / --color-line / --color-muted),
   * so the light-mode hex fallbacks rendered: near-white body text on a
   * white card = washed-out values. No functional changes. */
  .stats {
    padding: var(--space-4) var(--space-5);
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .stats__head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-3);
  }
  .stats__head h2 {
    margin: 0;
    font-size: var(--type-size-xl);
    font-weight: 600;
    color: var(--color-text-strong);
  }
  .stats__controls {
    display: flex;
    align-items: center;
    gap: var(--space-4);
  }
  .stats__basis {
    display: inline-flex;
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    overflow: hidden;
  }
  .stats__basis-btn {
    padding: var(--space-1) var(--space-3);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 0;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }
  .stats__basis-btn:hover {
    color: var(--color-text-strong);
  }
  .stats__basis-btn.active {
    background: var(--color-surface-sunken);
    color: var(--color-text-strong);
    font-weight: 600;
  }
  .stats__period {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .stats__period select {
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    border-radius: 3px;
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    cursor: pointer;
  }
  .stats__period select:hover {
    border-color: var(--color-text-muted);
  }
  .stats__meta {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
    display: flex;
    gap: var(--space-4);
    flex-wrap: wrap;
    margin: 0;
  }
  .stats__meta strong {
    color: var(--color-text-secondary);
    font-weight: 600;
  }

  /* Card grid — gap + padding aligned with the list views. */
  .stats__cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
    gap: var(--space-3);
  }
  .stats__card {
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    background: var(--color-surface-raised);
    transition: border-color var(--motion-fade-in);
  }
  .stats__card:hover {
    border-color: var(--color-text-muted);
  }
  .stats__card h3 {
    margin: 0 0 var(--space-1);
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-secondary);
  }

  /* Value row: dim currency/dimension label · strong tabular value ·
   * muted count. The value carries the eye (ADR-0017 §3). */
  .stats__row {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
    margin: 0;
  }
  .stats__row > span:first-child {
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }
  .stats__row .num {
    margin-left: auto;
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    font-weight: 600;
    font-size: var(--type-size-lg);
    color: var(--color-text-strong);
  }
  .stats__row .muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }
  .stats__detail {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
    margin: 0;
  }

  /* MoM / YoY deltas — signed, coloured, with a direction arrow. */
  .stats__delta {
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    margin: 0;
  }
  .stats__delta-label {
    color: var(--color-text-muted);
    letter-spacing: 0.04em;
    margin-right: var(--space-1);
  }
  .delta {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-muted);
  }
  .delta.up {
    color: var(--color-signal-positive);
  }
  .delta.up::before {
    content: "▲ ";
  }
  .delta.down {
    color: var(--color-signal-negative);
  }
  .delta.down::before {
    content: "▼ ";
  }

  .stats__empty {
    color: var(--color-text-muted);
    font-style: italic;
    text-align: center;
    margin: 0;
    padding: var(--space-3);
  }

  .stats__breakdown,
  .stats__hygiene,
  .stats__annual,
  .stats__top article {
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-3);
    background: var(--color-surface-raised);
  }
  .stats__breakdown h3,
  .stats__hygiene h3,
  .stats__annual h3,
  .stats__top h3 {
    margin: 0 0 var(--space-2);
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-secondary);
  }
  .stats__table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-md);
    background: var(--color-surface-sunken);
  }
  .stats__table th {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .stats__table th,
  .stats__table td {
    padding: var(--space-1) var(--space-2);
    border-bottom: 1px solid var(--color-surface-divider);
    text-align: left;
  }
  .stats__table td {
    color: var(--color-text-primary);
  }
  .stats__table .num {
    text-align: right;
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-strong);
  }
  .stats__table th.num {
    color: var(--color-text-muted);
  }

  .stats__top {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
    gap: var(--space-3);
  }
  .stats__top ol {
    margin: 0;
    padding-left: var(--space-5);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .stats__top li {
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }
  .stats__top strong {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-strong);
  }
  .stats__top .muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }

  /* Hygiene flags — a leading dot reflects zero (OK, calm green) vs
   * non-zero (needs attention, amber). The count itself goes amber
   * when non-zero so the eye lands on the rows that need action. */
  .stats__hygiene ul {
    margin: 0;
    padding-left: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .stats__hygiene li {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }
  .stats__hygiene li::before {
    content: "";
    flex: 0 0 auto;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--color-signal-positive);
    opacity: 0.45;
    transform: translateY(-1px);
  }
  .stats__hygiene li.flag-nonzero::before {
    background: var(--color-signal-warning);
    opacity: 1;
  }
  .stats__hygiene strong {
    margin-left: auto;
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-strong);
  }
  .stats__hygiene li.flag-nonzero strong {
    color: var(--color-signal-warning);
  }

  .stats__deferred {
    margin-top: var(--space-2);
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }
  .stats__deferred summary {
    cursor: pointer;
    color: var(--color-text-secondary);
  }
  .stats__loading {
    color: var(--color-text-secondary);
    font-style: italic;
  }
  .stats__error {
    border: 1px solid var(--color-signal-negative);
    border-radius: 4px;
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    color: var(--color-text-primary);
  }
  .stats__error strong {
    color: var(--color-signal-negative);
  }
</style>
