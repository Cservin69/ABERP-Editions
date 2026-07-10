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
    type AgingPanel,
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
  // PR-223 / S227 — hygiene-row click-through ([[hulye-biztos]] —
  // a non-zero count must give the operator a one-click path to the
  // rows behind it, not a number they hand-translate into filters).
  import {
    clickTargetForFlag,
    type HygieneFlag,
  } from "../lib/hygiene-clickthrough";
  // S262 / PR-251 — aging-bucket display + click-through into the lists.
  import {
    AGING_BUCKETS,
    AGING_LABELS,
    bucketAmount,
    type AgingBucket,
  } from "../lib/aging";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState: LoadState = $state("idle");
  let errorMessage = $state<string | null>(null);
  let report: FinancialReport | null = $state(null);

  // Default to current month (empty string → backend chooses current
  // month). Operator can change via the dropdown.
  let periodOptions = $state(buildPeriodOptions(new Date()));
  let selectedPeriod = $state<string>(periodOptions[0]?.wire ?? "");
  let dateBasis: DateBasis = $state("teljesites");

  // S262 / PR-251 — sentinel select value for the custom-range arm. The
  // backend already parses `YYYY-MM-DD..YYYY-MM-DD` (reports::parse_period
  // Custom arm); the SPA just composes that wire string from two date
  // inputs. `__custom__` is NOT a wire value — picking it reveals the
  // pickers and defers the load until Apply.
  const CUSTOM_SENTINEL = "__custom__";
  let periodChoice = $state<string>(periodOptions[0]?.wire ?? ""); // drives the <select>
  let customFrom = $state<string>("");
  let customTo = $state<string>("");
  let customError = $state<string | null>(null);

  // S262 / PR-251 — operator-configurable top-N (default 10, clamped
  // 1..50 to match the backend ceiling). Reloads on change.
  let topN = $state<number>(10);

  onMount(() => {
    void load();
  });

  async function load() {
    loadState = "loading";
    errorMessage = null;
    try {
      const r = await getFinancialReport(selectedPeriod, dateBasis, topN);
      report = r;
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function onPeriodChange(e: Event) {
    const target = e.target as HTMLSelectElement;
    periodChoice = target.value;
    customError = null;
    if (target.value === CUSTOM_SENTINEL) {
      // Reveal the date pickers; wait for Apply before loading.
      return;
    }
    selectedPeriod = target.value;
    void load();
  }

  function applyCustomRange() {
    if (customFrom === "" || customTo === "") {
      customError = "Adj meg kezdő és záró dátumot. / Pick a start and end date.";
      return;
    }
    if (customTo < customFrom) {
      customError = "A záró dátum nem lehet a kezdő előtt. / End date is before start.";
      return;
    }
    customError = null;
    selectedPeriod = `${customFrom}..${customTo}`;
    void load();
  }

  function onTopNChange(e: Event) {
    const raw = Number((e.target as HTMLInputElement).value);
    const next = Number.isFinite(raw) ? Math.min(50, Math.max(1, Math.round(raw))) : 10;
    if (next === topN) return;
    topN = next;
    void load();
  }

  // S262 / PR-251 — deep-link an aging bucket into the matching invoice
  // list. Mutates the hash so App.svelte's hashchange router re-renders
  // into InvoiceList / IncomingInvoiceList, which read `?aging=` on mount
  // (see hygiene-clickthrough.parseInvoicesUrl). Zero-count buckets are
  // rendered static by the template (no handler), matching the hygiene
  // row posture.
  function onAgingClick(tab: "outgoing" | "incoming", bucket: AgingBucket) {
    window.location.hash = `#/invoices?tab=${tab}&aging=${bucket}`;
  }

  /** Percent of the currency-split bar a HUF-minor amount occupies. The
   * denominator is the snapshot-HUF total (HUF native + EUR-as-HUF); 0
   * when there is no revenue (the bar renders empty). */
  function splitPct(part: number, total: number): number {
    if (total <= 0) return 0;
    return (part / total) * 100;
  }

  function setDateBasis(next: DateBasis) {
    if (next === dateBasis) return;
    dateBasis = next;
    void load();
  }

  // PR-223 / S227 — click-through navigation for a hygiene row.
  // Mutates `window.location.hash` so the SPA's hash router fires its
  // existing `hashchange` listener (App.svelte's `subscribeRoute`),
  // re-rendering into InvoiceList / IncomingInvoiceList with the
  // URL-driven filter init that the list reads on mount. A `null`
  // target keeps the row static (today: AR-side past-deadline — see
  // `clickTargetForFlag` for why).
  function onHygieneClick(flag: HygieneFlag) {
    const target = clickTargetForFlag(flag);
    if (target === null) return;
    window.location.hash = target.hash;
  }

  /** Per-flag display label (Hungarian + English) sourced verbatim from
   * the pre-S227 markup; lifted into a typed helper so the renderer
   * stays a single `{#each HYGIENE_ROWS}` loop and the click-vs-static
   * branch reads as one CSS-classed `<li>` instead of eight near-
   * duplicate blocks. */
  interface HygieneRow {
    flag: HygieneFlag;
    label: string;
  }
  // PR-223 / S227 — closed-vocab table; mirrors `HygienePanel` fields
  // in `api.ts`. Re-order = re-order on screen.
  const HYGIENE_ROWS: readonly HygieneRow[] = [
    { flag: "outgoing_pending", label: "Pending drafts (outgoing)" },
    { flag: "outgoing_rejected", label: "Rejected by NAV" },
    { flag: "outgoing_abandoned", label: "Abandoned" },
    {
      flag: "restored_no_partner",
      label: "Restored rows with no partner link",
    },
    {
      flag: "outstanding_past_deadline",
      label: "Outstanding receivables past deadline",
    },
    {
      flag: "payable_past_deadline",
      label: "Outstanding payables past deadline",
    },
    { flag: "storno_chain", label: "Storno chain entries in period" },
    {
      flag: "modification_chain",
      label: "Modification chain entries in period",
    },
  ];

  function countForFlag(report: FinancialReport, flag: HygieneFlag): number {
    switch (flag) {
      case "outgoing_pending":
        return report.hygiene.outgoing_pending_count;
      case "outgoing_rejected":
        return report.hygiene.outgoing_rejected_count;
      case "outgoing_abandoned":
        return report.hygiene.outgoing_abandoned_count;
      case "restored_no_partner":
        return report.hygiene.restored_no_partner_count;
      case "outstanding_past_deadline":
        return report.hygiene.outstanding_past_deadline_count;
      case "payable_past_deadline":
        return report.hygiene.payable_past_deadline_count;
      case "storno_chain":
        return report.hygiene.storno_chain_count;
      case "modification_chain":
        return report.hygiene.modification_chain_count;
    }
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
          value={periodChoice}
          onchange={onPeriodChange}
        >
          {#each periodOptions as opt (opt.wire)}
            <option value={opt.wire}>{opt.label}</option>
          {/each}
          <option value={CUSTOM_SENTINEL}>Custom range… / Egyéni</option>
        </select>
      </label>
      <!-- S262 / PR-251 — top-N control for the customer/vendor lists. -->
      <label class="stats__period">
        Top-N
        <input
          type="number"
          min="1"
          max="50"
          aria-label="Top-N customers and vendors"
          value={topN}
          onchange={onTopNChange}
        />
      </label>
    </div>
  </header>

  {#if periodChoice === CUSTOM_SENTINEL}
    <!-- S262 / PR-251 — custom date-range pickers. Emits the backend's
         `YYYY-MM-DD..YYYY-MM-DD` period wire form on Apply. -->
    <div class="stats__custom" role="group" aria-label="Custom date range">
      <label>
        From / Kezdő
        <input type="date" bind:value={customFrom} aria-label="Range start" />
      </label>
      <label>
        To / Záró
        <input type="date" bind:value={customTo} aria-label="Range end" />
      </label>
      <button type="button" onclick={applyCustomRange}>Apply / Alkalmaz</button>
      {#if customError !== null}
        <span class="stats__custom-err" role="alert">{customError}</span>
      {/if}
    </div>
  {/if}

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

    <!-- Row 2b: currency split (snapshot-rate HUF). S262 / PR-251 -->
    {@const cs = r.currency_split}
    {@const splitTotal = cs.huf_minor + cs.eur_as_huf_minor}
    <section class="stats__split" aria-label="Revenue currency split">
      <h3>Revenue currency split / Bevétel pénznem szerint</h3>
      {#if splitTotal <= 0}
        <p class="stats__empty">— no native outgoing revenue in this period —</p>
      {:else}
        <div
          class="split-bar"
          role="img"
          aria-label={`HUF ${splitPct(cs.huf_minor, splitTotal).toFixed(0)} percent, EUR ${splitPct(cs.eur_as_huf_minor, splitTotal).toFixed(0)} percent of HUF-equivalent revenue`}
        >
          {#if cs.huf_minor > 0}
            <div
              class="split-seg split-seg--huf"
              style={`width:${splitPct(cs.huf_minor, splitTotal)}%`}
            ></div>
          {/if}
          {#if cs.eur_as_huf_minor > 0}
            <div
              class="split-seg split-seg--eur"
              style={`width:${splitPct(cs.eur_as_huf_minor, splitTotal)}%`}
            ></div>
          {/if}
        </div>
        <ul class="split-legend">
          <li>
            <span class="split-dot split-dot--huf" aria-hidden="true"></span>
            HUF <strong>{formatHuf(cs.huf_minor)}</strong>
            <span class="muted">{splitPct(cs.huf_minor, splitTotal).toFixed(0)}% · ({cs.huf_count})</span>
          </li>
          <li>
            <span class="split-dot split-dot--eur" aria-hidden="true"></span>
            EUR <strong>{formatMinor(cs.eur_native_minor, "EUR")}</strong>
            <span class="muted">→ {formatHuf(cs.eur_as_huf_minor)} · {splitPct(cs.eur_as_huf_minor, splitTotal).toFixed(0)}% · ({cs.eur_count})</span>
          </li>
        </ul>
        <p class="stats__detail">
          EUR converted at each invoice's snapshot MNB rate (issuance, not
          today); native outgoing invoices only.
        </p>
      {/if}
    </section>

    <!-- Row 2c: AR + AP aging, click-through to filtered lists. S262 -->
    {#snippet agingPanel(title: string, panel: AgingPanel, tab: "outgoing" | "incoming")}
      <section class="stats__aging" aria-label={title}>
        <h3>{title}</h3>
        <ul class="aging-list">
          {#each AGING_BUCKETS as bucket (bucket)}
            {@const amt = bucketAmount(panel, bucket)}
            {#if amt.count > 0}
              <li class="aging-row clickable">
                <button
                  type="button"
                  class="aging-row-btn"
                  onclick={() => onAgingClick(tab, bucket)}
                  title={`${AGING_LABELS[bucket]} — kattints a szűrt listához. / Click to open the filtered list.`}
                  aria-label={`${AGING_LABELS[bucket]}: ${amt.count} invoices. Open the filtered list.`}
                >
                  <span class="aging-label">{AGING_LABELS[bucket]}</span>
                  <strong class="aging-count">{amt.count}</strong>
                  <span class="aging-amount">{formatHuf(amt.gross_minor)}*</span>
                  <span class="aging-chevron" aria-hidden="true">›</span>
                </button>
              </li>
            {:else}
              <li class="aging-row">
                <span class="aging-label">{AGING_LABELS[bucket]}</span>
                <strong class="aging-count">0</strong>
                <span class="aging-amount">—</span>
              </li>
            {/if}
          {/each}
        </ul>
        <p class="stats__detail">* counts are exact; amounts sum HUF + EUR.</p>
      </section>
    {/snippet}
    <section class="stats__aging-grid" aria-label="Aging">
      {@render agingPanel(
        "Receivables aging / Vevőkövetelés korosítás",
        r.receivables_aging,
        "outgoing",
      )}
      {@render agingPanel(
        "Payables aging / Szállítói tartozás korosítás",
        r.payables_aging,
        "incoming",
      )}
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

    <!-- Row 5: Hygiene flags. PR-223 / S227 — each non-zero row is a
         click target into InvoiceList / IncomingInvoiceList, pre-
         filtered to the rows behind the count. Zero-count rows stay
         static (no chevron, no hover). The two AR-side past-deadline
         flags currently have no exact row-level filter (the wire
         shape `InvoiceListItem` does not carry `payment_deadline`,
         and adding it is out of S227 scope) — those rows stay static
         even when non-zero. -->
    <section class="stats__hygiene" aria-label="Hygiene flags">
      <h3>Hygiene</h3>
      <ul>
        {#each HYGIENE_ROWS as row (row.flag)}
          {@const count = countForFlag(r, row.flag)}
          {@const target = clickTargetForFlag(row.flag)}
          {@const clickable = count > 0 && target !== null}
          {#if clickable && target !== null}
            <li class="flag-nonzero clickable">
              <button
                type="button"
                class="hygiene-row-btn"
                onclick={() => onHygieneClick(row.flag)}
                aria-label={`${row.label}: ${count}. Open list filtered to these rows.`}
                title={`${row.label} — kattints a szűrt listához. / Click to open the filtered list.`}
              >
                <span class="hygiene-label">{row.label}:</span>
                <strong>{count}</strong>
                <span class="hygiene-chevron" aria-hidden="true">›</span>
              </button>
            </li>
          {:else}
            <li class:flag-nonzero={count > 0}>
              <span class="hygiene-label">{row.label}:</span>
              <strong>{count}</strong>
            </li>
          {/if}
        {/each}
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
    border-radius: var(--radius-sm);
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
    border-radius: var(--radius-sm);
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
    border-radius: var(--radius-sm);
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
    border-radius: var(--radius-sm);
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
    border-radius: var(--radius-full);
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

  /* PR-223 / S227 — clickable hygiene row chrome. The button strips
   * its native background + border so the row reads visually
   * identical to the static span path; the chevron + hover state
   * carry the only affordance signal (ADR-0017 §1-2 quiet chrome).
   * `display: contents` is intentional — the inner button must
   * inherit the parent `<li>`'s flex layout (the leading dot,
   * label, count, chevron all sit on the same row), not introduce
   * its own block box. */
  .stats__hygiene .hygiene-row-btn {
    display: contents;
    background: none;
    border: 0;
    padding: 0;
    margin: 0;
    font: inherit;
    color: inherit;
    cursor: pointer;
    text-align: inherit;
  }
  .stats__hygiene li.clickable {
    cursor: pointer;
    transition: color var(--motion-fade-in);
  }
  .stats__hygiene li.clickable:hover .hygiene-label {
    color: var(--color-text-strong);
  }
  .stats__hygiene li.clickable:hover strong {
    color: var(--color-text-strong);
  }
  .stats__hygiene li.clickable.flag-nonzero:hover strong {
    /* When the count is in the warning colour AND we're hovering,
     * keep the warning colour so the eye doesn't see a colour
     * change that suggests "this is now safe". */
    color: var(--color-signal-warning);
    filter: brightness(1.2);
  }
  .stats__hygiene .hygiene-chevron {
    color: var(--color-text-muted);
    font-size: var(--type-size-md);
    line-height: 1;
    margin-left: var(--space-1);
    transition: transform var(--motion-fade-in);
  }
  .stats__hygiene li.clickable:hover .hygiene-chevron {
    color: var(--color-text-strong);
    transform: translateX(2px);
  }
  .stats__hygiene .hygiene-row-btn:focus-visible {
    outline: 2px solid var(--color-text-strong);
    outline-offset: 2px;
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
    border-radius: var(--radius-sm);
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    color: var(--color-text-primary);
  }
  .stats__error strong {
    color: var(--color-signal-negative);
  }

  /* S262 / PR-251 — top-N number input matches the period select chrome. */
  .stats__period input[type="number"] {
    width: 4.5rem;
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  /* Custom date-range pickers. */
  .stats__custom {
    display: flex;
    align-items: flex-end;
    flex-wrap: wrap;
    gap: var(--space-3);
    padding: var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
  }
  .stats__custom label {
    display: inline-flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .stats__custom input[type="date"] {
    background: var(--color-surface-sunken);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }
  .stats__custom button {
    background: var(--color-surface-sunken);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    padding: var(--space-1) var(--space-3);
    cursor: pointer;
    font-size: var(--type-size-sm);
  }
  .stats__custom button:hover {
    border-color: var(--color-text-muted);
  }
  .stats__custom-err {
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm);
    align-self: center;
  }

  /* Currency split — stacked bar + legend. */
  .stats__split {
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    padding: var(--space-3);
    background: var(--color-surface-raised);
  }
  .stats__split h3 {
    margin: 0 0 var(--space-2);
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-secondary);
  }
  .split-bar {
    display: flex;
    width: 100%;
    height: 18px;
    border-radius: var(--radius-sm);
    overflow: hidden;
    background: var(--color-surface-sunken);
  }
  .split-seg {
    height: 100%;
  }
  .split-seg--huf {
    background: var(--color-signal-positive);
  }
  .split-seg--eur {
    background: var(--color-signal-warning);
  }
  .split-legend {
    list-style: none;
    margin: var(--space-2) 0 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .split-legend strong {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-strong);
  }
  .split-legend .muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }
  .split-dot {
    display: inline-block;
    width: 9px;
    height: 9px;
    border-radius: var(--radius-sm);
    margin-right: var(--space-1);
  }
  .split-dot--huf {
    background: var(--color-signal-positive);
  }
  .split-dot--eur {
    background: var(--color-signal-warning);
  }

  /* Aging — two side-by-side panels of clickable bucket rows. */
  .stats__aging-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
    gap: var(--space-3);
  }
  .stats__aging {
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    padding: var(--space-3);
    background: var(--color-surface-raised);
  }
  .stats__aging h3 {
    margin: 0 0 var(--space-2);
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-secondary);
  }
  .aging-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .aging-row {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .aging-row-btn {
    display: flex;
    width: 100%;
    align-items: baseline;
    gap: var(--space-2);
    background: none;
    border: 0;
    padding: var(--space-1) 0;
    margin: 0;
    font: inherit;
    color: inherit;
    cursor: pointer;
    text-align: inherit;
  }
  .aging-label {
    color: var(--color-text-secondary);
  }
  .aging-count {
    margin-left: auto;
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-strong);
  }
  .aging-amount {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    min-width: 6rem;
    text-align: right;
  }
  .aging-chevron {
    color: var(--color-text-muted);
    transition: transform var(--motion-fade-in);
  }
  .aging-row.clickable:hover .aging-label,
  .aging-row.clickable:hover .aging-count {
    color: var(--color-text-strong);
  }
  .aging-row.clickable:hover .aging-chevron {
    color: var(--color-text-strong);
    transform: translateX(2px);
  }
  .aging-row-btn:focus-visible {
    outline: 2px solid var(--color-text-strong);
    outline-offset: 2px;
  }
</style>
