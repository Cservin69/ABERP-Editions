<script lang="ts">
  // S211 / PR-210 — Quotes tab. Surfaces the daemon-staged
  // `quote_intake_log` rows for the operator. Read-only in S211: the
  // pickup action (operator-clicks → opens InvoiceCompose pre-populated
  // from the prepared draft) lands in S212. Until then, every visible
  // row carries the "Needs operator pickup" chip.
  //
  // Per S179's pattern, this is a third tab under Invoices alongside
  // Outgoing / Incoming — operator-clearer adjacency (quote-intake
  // produces draft invoices).
  //
  // Conservative-choice flags (S211b):
  //   - sort/filter persistence DELIBERATELY OUT OF SCOPE — the list
  //     is sorted by intake_at DESC server-side; the SPA does no
  //     additional re-ranking. S212 may add facets once the operator
  //     queue grows.
  //   - "Open invoice" link is disabled — there is NO billing.invoice
  //     row for these draft ids in S211 (the daemon stages a prepared
  //     draft JSON; the operator-clicked pickup creates the row).

  import { onMount } from "svelte";
  import { listQuoteIntake, type QuoteIntakeRow } from "../lib/api";
  import { formatInvoiceDate } from "../lib/format";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState: LoadState = $state("idle");
  let errorMessage = $state<string | null>(null);
  let rows: QuoteIntakeRow[] = $state([]);

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      rows = await listQuoteIntake();
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function shortQuoteId(id: string): string {
    if (id.length <= 12) return id;
    return `${id.slice(0, 6)}…${id.slice(-4)}`;
  }

  function writebackLabel(ts: string | null): {
    hu: string;
    en: string;
    tone: "pending" | "done";
  } {
    if (ts === null) {
      return {
        hu: "Visszajelzés függőben",
        en: "Writeback pending",
        tone: "pending",
      };
    }
    return {
      hu: "✓ Visszajelzés rendben",
      en: "Writeback complete",
      tone: "done",
    };
  }

  function fmt(ts: string): string {
    // Mirror IncomingInvoiceList: `formatInvoiceDate` handles the
    // common "yyyy-MM-dd" + "yyyy-MM-ddTHH:mm:ssZ" cases.
    return formatInvoiceDate(ts);
  }
</script>

<section class="quotes-page" data-testid="quotes-list-section">
  <header class="quotes-page__head">
    <div>
      <h2 class="quotes-page__title">
        Ajánlatok / Quotes
        <span class="quotes-page__hint">
          ABERP-site-ról beérkezett, operátorra váró ajánlatok
        </span>
      </h2>
    </div>
    <div class="quotes-page__actions">
      <button
        type="button"
        class="quotes-page__refresh"
        disabled={loadState === "loading"}
        onclick={() => void refresh()}
        data-testid="quotes-refresh"
      >
        {loadState === "loading" ? "Frissítés…" : "Frissítés / Refresh"}
      </button>
    </div>
  </header>

  {#if loadState === "loading" && rows.length === 0}
    <p class="quotes-page__muted">Betöltés… / Loading quotes…</p>
  {:else if loadState === "error"}
    <div class="quotes-page__error" role="alert">
      <strong>Nem sikerült lekérni az ajánlatokat / Failed to load quotes.</strong>
      <p class="quotes-page__error-detail">{errorMessage}</p>
    </div>
  {:else if rows.length === 0}
    <div class="quotes-page__empty" data-testid="quotes-empty">
      <p>
        Nincs még beérkezett ajánlat. Aktiváld az ajánlatfeladás daemont
        a Tenant beállítások &rarr; Ajánlatfeladás szekciónál (és indítsd
        újra az ABERP-et a változás érvényesítéséhez).
      </p>
      <p>
        No quotes staged yet. Enable the quote-intake daemon in
        Tenant Settings &rarr; Quote Intake (and restart ABERP for the
        change to take effect).
      </p>
    </div>
  {:else}
    <table class="quotes-table" data-testid="quotes-table">
      <thead>
        <tr>
          <th scope="col">Beérkezett / Received</th>
          <th scope="col">Stage-elve / Staged</th>
          <th scope="col">Ajánlat / Quote</th>
          <th scope="col">Vevő / Contact</th>
          <th scope="col">Anyag / Material</th>
          <th scope="col" class="quotes-table__num">Db / Qty</th>
          <th scope="col">Visszajelzés / Writeback</th>
          <th scope="col">Művelet / Action</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row (row.quote_id)}
          {@const wb = writebackLabel(row.status_writeback_at)}
          <tr data-testid="quotes-row" data-quote-id={row.quote_id}>
            <td>{fmt(row.received_at)}</td>
            <td>{fmt(row.intake_at)}</td>
            <td>
              <code
                class="quotes-table__qid"
                title={row.quote_id}
                data-testid="quotes-row-id"
              >{shortQuoteId(row.quote_id)}</code>
            </td>
            <td>
              {#if row.contact_name}
                <div class="quotes-table__contact-name">{row.contact_name}</div>
              {/if}
              {#if row.contact_company}
                <div class="quotes-table__contact-company">{row.contact_company}</div>
              {/if}
              {#if row.contact_email}
                <div class="quotes-table__contact-email">{row.contact_email}</div>
              {/if}
              {#if !row.contact_name && !row.contact_email && !row.contact_company}
                <span class="quotes-table__muted">—</span>
              {/if}
            </td>
            <td>
              {#if row.material}
                <div>{row.material}</div>
              {:else}
                <span class="quotes-table__muted">—</span>
              {/if}
              {#if row.notes}
                <div
                  class="quotes-table__notes"
                  title={row.notes}
                  data-testid="quotes-row-notes"
                >{row.notes}</div>
              {/if}
            </td>
            <td class="quotes-table__num">{row.quantity ?? "—"}</td>
            <td>
              <span
                class="quotes-chip quotes-chip--{wb.tone}"
                data-testid="quotes-writeback-chip"
                title={wb.en}
              >{wb.hu}</span>
            </td>
            <td>
              <span
                class="quotes-chip quotes-chip--attention"
                data-testid="quotes-attention-chip"
                title="Operator pickup creates the draft invoice (S212)"
              >Operátori beavatkozás szükséges</span>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  /* S226 / PR-222 — dark-theme colour polish. Same root cause as
   * StatisticsPage: this page (S211b/PR-210) referenced undefined token
   * names (--color-muted / --color-border / --color-surface[-alt] /
   * --text-* / --font-mono / --color-error*) and a handful of light-mode
   * hex literals, so it rendered washed-out on the dark theme. Every
   * colour now resolves to a tokens.css variable (ADR-0017); no new
   * tokens; no functional change. */
  .quotes-page {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) 0;
  }

  .quotes-page__head {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: var(--space-3);
    flex-wrap: wrap;
  }

  .quotes-page__title {
    font-size: var(--type-size-lg);
    font-weight: 600;
    margin: 0;
    color: var(--color-text-strong);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .quotes-page__hint {
    font-size: var(--type-size-sm);
    font-weight: 400;
    color: var(--color-text-muted);
  }

  .quotes-page__actions {
    display: flex;
    gap: var(--space-2);
  }

  .quotes-page__refresh {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .quotes-page__refresh:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quotes-page__refresh:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .quotes-page__muted {
    color: var(--color-text-muted);
    font-style: italic;
  }

  .quotes-page__error {
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-negative);
    border-radius: 3px;
    color: var(--color-text-primary);
  }

  .quotes-page__error strong {
    color: var(--color-signal-negative);
  }

  .quotes-page__error-detail {
    margin-top: var(--space-1);
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    color: var(--color-text-muted);
  }

  .quotes-page__empty {
    padding: var(--space-4);
    background: var(--color-surface-raised);
    border: 1px dashed var(--color-surface-divider);
    border-radius: 3px;
    color: var(--color-text-secondary);
  }

  .quotes-page__empty p + p {
    margin-top: var(--space-2);
  }

  .quotes-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
    background: var(--color-surface-sunken);
  }

  .quotes-table th,
  .quotes-table td {
    padding: var(--space-2) var(--space-3);
    text-align: left;
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
  }

  .quotes-table td {
    color: var(--color-text-primary);
  }

  .quotes-table th {
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .quotes-table tbody tr:hover {
    background: var(--color-surface-raised);
  }

  .quotes-table__num {
    text-align: right;
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-strong);
  }

  .quotes-table th.quotes-table__num {
    color: var(--color-text-muted);
  }

  .quotes-table__qid {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .quotes-table__contact-name {
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .quotes-table__contact-company {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .quotes-table__contact-email {
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
    color: var(--color-text-secondary);
  }

  .quotes-table__notes {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    max-width: 22rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .quotes-table__muted {
    color: var(--color-text-muted);
  }

  /* Status chips — categorical signal (ADR-0017 §"the 20%"): a coloured
   * label + matching hairline on a raised surface, no light fills. */
  .quotes-chip {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    font-weight: 500;
    white-space: nowrap;
  }

  .quotes-chip--pending {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  .quotes-chip--done {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  .quotes-chip--attention {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
</style>
