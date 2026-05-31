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
    font-size: var(--text-lg);
    font-weight: 600;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .quotes-page__hint {
    font-size: var(--text-sm);
    font-weight: 400;
    color: var(--color-muted);
  }

  .quotes-page__actions {
    display: flex;
    gap: var(--space-2);
  }

  .quotes-page__refresh {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-border);
    background: var(--color-surface);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--text-sm);
  }

  .quotes-page__refresh:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .quotes-page__muted {
    color: var(--color-muted);
    font-style: italic;
  }

  .quotes-page__error {
    padding: var(--space-3);
    background: var(--color-error-bg, #fee);
    border: 1px solid var(--color-error, #c00);
    border-radius: var(--radius-sm);
    color: var(--color-error, #c00);
  }

  .quotes-page__error-detail {
    margin-top: var(--space-1);
    font-size: var(--text-sm);
    font-family: var(--font-mono, monospace);
  }

  .quotes-page__empty {
    padding: var(--space-4);
    background: var(--color-surface-alt, #f7f7f7);
    border-radius: var(--radius-sm);
    color: var(--color-muted);
  }

  .quotes-page__empty p + p {
    margin-top: var(--space-2);
  }

  .quotes-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--text-sm);
  }

  .quotes-table th,
  .quotes-table td {
    padding: var(--space-2) var(--space-3);
    text-align: left;
    border-bottom: 1px solid var(--color-border);
    vertical-align: top;
  }

  .quotes-table th {
    background: var(--color-surface-alt, #f7f7f7);
    font-weight: 600;
  }

  .quotes-table__num {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  .quotes-table__qid {
    font-family: var(--font-mono, monospace);
    font-size: var(--text-xs);
  }

  .quotes-table__contact-name {
    font-weight: 600;
  }

  .quotes-table__contact-company {
    font-size: var(--text-xs);
    color: var(--color-muted);
  }

  .quotes-table__contact-email {
    font-size: var(--text-xs);
    font-family: var(--font-mono, monospace);
  }

  .quotes-table__notes {
    font-size: var(--text-xs);
    color: var(--color-muted);
    max-width: 22rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .quotes-table__muted {
    color: var(--color-muted);
  }

  .quotes-chip {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 999px;
    font-size: var(--text-xs);
    font-weight: 500;
    white-space: nowrap;
  }

  .quotes-chip--pending {
    background: #fff4d6;
    color: #7a5a00;
  }

  .quotes-chip--done {
    background: #d8f5e6;
    color: #0b5a2b;
  }

  .quotes-chip--attention {
    background: #fce4d6;
    color: #8a2a00;
  }
</style>
