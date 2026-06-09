<script lang="ts">
  // S279 / PR-265 — Pricing tab. Read-only operator view of the
  // ABERP-side auto-quoting producer pipeline (Fetched → Extracting →
  // Pricing → Rendering → PostingBack → Posted / Failed). The daemon is
  // the only writer; the SPA shows progress + the per-row "Retry" button
  // on Failed rows.
  //
  // Distinct tab from `quotes` (operator-pickup queue for approved quotes
  // awaiting DEAL). This tab is "pricing in flight"; the other is "deal
  // it or pick it up."

  import { onMount } from "svelte";
  import {
    fetchQuotePipelineStatus,
    listQuotePricingJobs,
    retryQuotePricingJob,
    type PipelinePythonStatus,
    type PricingJobRow,
  } from "../lib/api";
  import { formatInvoiceDate } from "../lib/format";
  import { classifyEmptyState } from "../lib/pricing-empty-state";
  import { failureKindBadge } from "../lib/pricing-failure-kind";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState: LoadState = $state("idle");
  let errorMessage = $state<string | null>(null);
  let rows: PricingJobRow[] = $state([]);
  let retryBusyQuoteId = $state<string | null>(null);
  let retryError = $state<string | null>(null);
  // S282 / PR-267 — pipeline daemon status. Fetched in parallel with the
  // jobs list so the empty-state copy can differentiate dormant /
  // active / errored. Null until first fetch returns.
  let pipelineStatus = $state<PipelinePythonStatus | null>(null);

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      const [r, s] = await Promise.all([
        listQuotePricingJobs(),
        fetchQuotePipelineStatus().catch(() => null),
      ]);
      rows = r;
      pipelineStatus = s;
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

  function stateChipClass(state: string): string {
    switch (state) {
      case "posted":
        return "chip chip--ok";
      case "failed":
        return "chip chip--err";
      case "fetched":
        return "chip chip--queued";
      default:
        return "chip chip--running";
    }
  }

  function stateLabel(state: string): string {
    switch (state) {
      case "fetched":
        return "Beérkezett / Fetched";
      case "extracting":
        return "CAD-elemzés / Extracting";
      case "pricing":
        return "Árazás / Pricing";
      case "rendering":
        return "PDF / Rendering";
      case "posting_back":
        return "Visszaküldés / Posting back";
      case "posted":
        return "Elküldve / Posted";
      case "failed":
        return "Sikertelen / Failed";
      default:
        return state;
    }
  }

  // S290 / PR-271 — failure-kind badge rendering is extracted to
  // `lib/pricing-failure-kind.ts` so the four-branch logic can be
  // vitest-pinned without component-render tooling. Imported above.

  async function onRetry(quoteId: string): Promise<void> {
    retryBusyQuoteId = quoteId;
    retryError = null;
    try {
      await retryQuotePricingJob(quoteId);
      await refresh();
    } catch (e) {
      retryError = e instanceof Error ? e.message : String(e);
    } finally {
      retryBusyQuoteId = null;
    }
  }
</script>

<section class="pricing-jobs">
  <header class="pricing-jobs__hdr">
    <div>
      <h2>Auto-quoting pipeline / Auto-árazás folyamatban</h2>
      <p class="pricing-jobs__sub">
        Storefrontról beérkezett ajánlatok ABERP-oldali árazás közben /
        Storefront submissions being priced by ABERP's daemon
      </p>
    </div>
    <button
      type="button"
      class="btn btn--secondary"
      onclick={() => void refresh()}
      disabled={loadState === "loading"}
      data-testid="pricing-jobs-refresh"
    >Frissítés / Refresh</button>
  </header>

  {#if pipelineStatus && pipelineStatus.recent_panic_count > 0}
    <!-- S286 / PR-268 — AMBER daemon-health banner. Persists above the
         table when the supervisor caught any Rust-side panics in the
         last 10 minutes; the audit ledger has the durable forensic
         detail. Renders regardless of rows.length so the operator sees
         it even while the daemon is mid-cycle and the table populated. -->
    <div class="pricing-jobs__warn" data-testid="pricing-jobs-daemon-panic-banner">
      <p>
        <strong>Daemon recovered from {pipelineStatus.recent_panic_count}
          {pipelineStatus.recent_panic_count === 1 ? "panic" : "panics"} in the last 10 minutes /
          Daemon
          {pipelineStatus.recent_panic_count === 1 ? "egy" : pipelineStatus.recent_panic_count}
          összeomlásból tért magához az elmúlt 10 percben.</strong>
      </p>
      {#if pipelineStatus.last_panic_at}
        <p>
          Utolsó / Last: <code>{pipelineStatus.last_panic_at}</code>
        </p>
      {/if}
      <p>
        Lásd az audit naplóban: <code>quote.pricing_daemon_panicked</code> /
        See the audit ledger.
      </p>
    </div>
  {/if}
  {#if loadState === "loading" && rows.length === 0}
    <p class="pricing-jobs__hint">Betöltés… / Loading…</p>
  {:else if loadState === "error"}
    <p class="pricing-jobs__err">{errorMessage ?? "Hiba / Error"}</p>
  {:else if classifyEmptyState(rows.length, pipelineStatus) === "venv_disabled_by_operator"}
    <!-- S286 / PR-268 — AMBER card: venv manually moved aside by the
         operator (e.g. Ervin's `mv .venv .venv.disabled-pending-hotfix`
         mitigation after the PROD_v2.27.2 crash). Distinct from
         "venv missing" so the operator sees their own action surfaced. -->
    <div class="pricing-jobs__warn" data-testid="pricing-jobs-empty-operator-disabled">
      <p>
        <strong>Daemon dormant — venv was renamed by operator /
          Daemon szünetel — a venv-et az üzemeltető átnevezte.</strong>
      </p>
      <p>
        Renamed to / Átnevezve:
        <code>{pipelineStatus?.operator_disabled_path ?? "?"}</code>
      </p>
      <p>
        Rename back to <code>.venv</code> when the hotfix is in place. /
        Nevezd vissza <code>.venv</code>-re, ha a javítás megérkezett.
      </p>
    </div>
  {:else if classifyEmptyState(rows.length, pipelineStatus) === "venv_missing"}
    <!-- S282 / PR-267 — RED card: venv not provisioned. Operator-actionable. -->
    <div class="pricing-jobs__err" data-testid="pricing-jobs-empty-not-resolved">
      <p>
        <strong>Daemon dormant — Python venv not detected /
          Daemon szünetel — Python venv nem található.</strong>
      </p>
      <p>
        Expected at / Várt útvonal:
        <code>{pipelineStatus?.canonical_path ?? "?"}</code>
      </p>
      <p>
        Run / Futtasd:
        <code>./run/upgrade_prod.sh</code>
        to provision / a venv telepítéséhez.
      </p>
    </div>
  {:else if classifyEmptyState(rows.length, pipelineStatus) === "spawn_errored"}
    <!-- S282 / PR-267 — AMBER card: venv resolved but daemon spawn errored. -->
    <div class="pricing-jobs__warn" data-testid="pricing-jobs-empty-spawn-errored">
      <p>
        <strong>Daemon failed to start / Daemon nem indult el.</strong>
      </p>
      <p>
        Resolved at / Megtalálva: <code>{pipelineStatus?.resolved_path ?? "?"}</code>
      </p>
      <p>See backend logs for detail / Részletekért lásd a backend logokat.</p>
    </div>
  {:else if rows.length === 0}
    <!-- S282 / PR-267 — GREEN card: active, polling, no pending work. -->
    <p class="pricing-jobs__hint" data-testid="pricing-jobs-empty-active">
      {#if pipelineStatus?.poll_cadence_secs}
        Daemon aktív — {pipelineStatus.poll_cadence_secs} másodpercenként lekérdez. /
        Daemon active — polling every {pipelineStatus.poll_cadence_secs}s.
        Nincs függő ajánlat a storefronton. / No pending submissions on storefront.
      {:else}
        Nincs aktív árazási feladat. / No pricing jobs in flight.
      {/if}
    </p>
  {:else}
    {#if retryError}
      <p class="pricing-jobs__err" data-testid="pricing-jobs-retry-error">
        {retryError}
      </p>
    {/if}
    <table class="pricing-jobs__tbl" data-testid="pricing-jobs-table">
      <thead>
        <tr>
          <th>Ref / Ref</th>
          <th>Vevő / Customer</th>
          <th>Anyag / Material</th>
          <th>Db / Qty</th>
          <th>Állapot / State</th>
          <th>Ár / Price (EUR)</th>
          <th>Hiba / Error</th>
          <th>Frissítve / Updated</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row (row.quote_id)}
          <tr data-testid={`pricing-jobs-row-${row.quote_id}`}>
            <td>
              <code title={row.quote_id}>{shortQuoteId(row.quote_id)}</code>
              {#if row.attempt_n > 0}
                <span class="pricing-jobs__attempt">×{row.attempt_n}</span>
              {/if}
            </td>
            <td>
              <div>{row.customer_name}</div>
              <div class="pricing-jobs__muted">{row.customer_email}</div>
            </td>
            <td>{row.material_grade}</td>
            <td>{row.quantity}</td>
            <td>
              <span class={stateChipClass(row.state)}>
                {stateLabel(row.state)}
              </span>
            </td>
            <td>
              {#if row.total_price_eur !== null && row.total_price_eur !== undefined}
                {row.total_price_eur.toFixed(2)}
              {:else}
                —
              {/if}
            </td>
            <td>
              {#if row.state === "failed"}
                <div class="pricing-jobs__err-stage">{row.error_stage ?? ""}</div>
                {#if failureKindBadge(row.failure_kind, row.error_reason)}
                  {@const badge = failureKindBadge(row.failure_kind, row.error_reason)!}
                  <div class="pricing-jobs__kind-row">
                    <span
                      class={badge.className}
                      data-testid={`pricing-jobs-kind-${row.quote_id}`}
                      data-failure-kind={row.failure_kind ?? "null"}
                    >{badge.label}</span>
                  </div>
                {/if}
                <div class="pricing-jobs__muted">{row.error_reason ?? ""}</div>
              {:else}
                —
              {/if}
            </td>
            <td>{formatInvoiceDate(row.updated_at)}</td>
            <td>
              {#if row.state === "failed"}
                <button
                  type="button"
                  class="btn btn--secondary"
                  onclick={() => void onRetry(row.quote_id)}
                  disabled={retryBusyQuoteId === row.quote_id}
                  data-testid={`pricing-jobs-retry-${row.quote_id}`}
                >
                  {retryBusyQuoteId === row.quote_id
                    ? "Újrapróbálás… / Retrying…"
                    : "Újra / Retry"}
                </button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  .pricing-jobs {
    color: var(--color-text, #e5e7eb);
    padding: 16px;
  }
  .pricing-jobs__hdr {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: 16px;
    margin-bottom: 12px;
  }
  .pricing-jobs__sub {
    font-size: 12px;
    color: var(--color-text-muted, #9ca3af);
    margin: 4px 0 0;
  }
  .pricing-jobs__hint,
  .pricing-jobs__err {
    padding: 12px;
    border: 1px solid var(--color-border, #374151);
    border-radius: 6px;
    background: var(--color-surface, #1f2937);
  }
  .pricing-jobs__err {
    border-color: var(--color-danger, #f87171);
    color: var(--color-danger, #f87171);
  }
  /* S282 / PR-267 — AMBER card for resolved-but-spawn-errored state.
     Distinct from RED so the operator can tell "missing" from "broken". */
  .pricing-jobs__warn {
    padding: 12px;
    border: 1px solid var(--color-warn, #f59e0b);
    border-radius: 6px;
    background: var(--color-surface, #1f2937);
    color: var(--color-warn, #f59e0b);
  }
  .pricing-jobs__tbl {
    width: 100%;
    border-collapse: collapse;
  }
  .pricing-jobs__tbl th,
  .pricing-jobs__tbl td {
    text-align: left;
    padding: 8px 10px;
    border-bottom: 1px solid var(--color-border, #374151);
    font-size: 13px;
    vertical-align: top;
  }
  .pricing-jobs__tbl th {
    font-weight: 600;
    color: var(--color-text-muted, #9ca3af);
  }
  .pricing-jobs__muted {
    color: var(--color-text-muted, #9ca3af);
    font-size: 12px;
  }
  .pricing-jobs__err-stage {
    font-weight: 600;
    color: var(--color-danger, #f87171);
  }
  /* S290 / PR-271 — gives the failure-kind badge breathing room above
     the (often long) error_reason line. */
  .pricing-jobs__kind-row {
    margin: 4px 0;
  }
  .pricing-jobs__attempt {
    margin-left: 6px;
    font-size: 11px;
    color: var(--color-text-muted, #9ca3af);
  }
  .chip {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 500;
    background: #374151;
    color: #f3f4f6;
  }
  .chip--ok {
    background: #064e3b;
    color: #bbf7d0;
  }
  .chip--err {
    background: #7f1d1d;
    color: #fecaca;
  }
  .chip--queued {
    background: #1e3a8a;
    color: #bfdbfe;
  }
  .chip--running {
    background: #78350f;
    color: #fed7aa;
  }
</style>
