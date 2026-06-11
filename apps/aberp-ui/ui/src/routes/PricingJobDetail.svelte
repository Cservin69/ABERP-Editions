<script lang="ts">
  // S349 / PR-40 (U1) — Auto-Quoting row-detail panel. Opens when the
  // operator clicks a row in `PricingJobsList.svelte`; shows EVERYTHING
  // needed to act on one quote at 5am: submission metadata, CAD +
  // material, the pricing breakdown, the extracted FeatureGraph, the
  // derived status timeline, the last writeback outcome, and the
  // paginated per-row audit trail.
  //
  // Mirrors `InvoiceDetail.svelte`'s native-`<dialog>` modal pattern:
  // ESC + backdrop-click + programmatic close all fire the native
  // `close` event, which we forward to `onClose` so the parent resets
  // its `selectedQuoteId` (closing on URL/tab change is automatic —
  // the parent route unmounts). Panel state lives in component `$state`
  // (in-memory Svelte state, never browser storage).

  import {
    getQuotePricingJob,
    getQuotePricingJobAudit,
    retryQuotePricingJob,
    type AuditEntryView,
    type PricingJobDetail,
  } from "../lib/api";
  import { formatInvoiceDate } from "../lib/format";
  import { failureKindBadge } from "../lib/pricing-failure-kind";
  import { writebackOutcomeBadge } from "../lib/pricing-failure-kind";
  import {
    auditKindLabel,
    breakdownRows,
    latestWritebackOutcome,
    timelineNodes,
  } from "../lib/pricing-job-detail";

  interface Props {
    /** Storefront quote id of the row to inspect; `null` keeps the
     *  modal closed. The parent rebinds this between a string and
     *  `null` to open/close. */
    quoteId: string | null;
    /** Forwarded from the native dialog `close` event so the parent
     *  resets its `selectedQuoteId` (so re-clicking the same row
     *  re-opens). */
    onClose: () => void;
    /** Invoked after a successful Retry so the parent list refreshes. */
    onRetried: () => void;
  }

  let { quoteId, onClose, onRetried }: Props = $props();

  type LoadState = "idle" | "loading" | "ready" | "error";

  let dialogEl: HTMLDialogElement | null = $state(null);
  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let detail = $state<PricingJobDetail | null>(null);
  let auditEvents = $state<AuditEntryView[]>([]);
  let auditTotal = $state(0);
  let auditLoading = $state(false);
  let retryBusy = $state(false);
  let retryError = $state<string | null>(null);
  let expandedSeqs = $state<Set<number>>(new Set());

  const AUDIT_PAGE = 50;

  // Derived sections — all read off the one audit page we fetch.
  const timeline = $derived(timelineNodes(auditEvents));
  const writeback = $derived(latestWritebackOutcome(auditEvents));
  const priceRows = $derived(breakdownRows(detail?.breakdown ?? null));

  // Open on a non-null quoteId; close + reset on null. Guard the
  // double-open InvalidStateError per the InvoiceDetail precedent.
  $effect(() => {
    if (!dialogEl) return;
    if (quoteId !== null) {
      if (!dialogEl.open) dialogEl.showModal();
      expandedSeqs = new Set();
      retryError = null;
      void load(quoteId);
    } else {
      if (dialogEl.open) dialogEl.close();
      detail = null;
      auditEvents = [];
      auditTotal = 0;
      loadState = "idle";
      errorMessage = null;
    }
  });

  async function load(id: string): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      const [d, page] = await Promise.all([
        getQuotePricingJob(id),
        getQuotePricingJobAudit(id, AUDIT_PAGE, 0).catch(() => ({
          events: [],
          total: 0,
          limit: AUDIT_PAGE,
          offset: 0,
        })),
      ]);
      detail = d;
      auditEvents = page.events;
      auditTotal = page.total;
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  async function loadMoreAudit(): Promise<void> {
    if (!quoteId) return;
    auditLoading = true;
    try {
      const page = await getQuotePricingJobAudit(
        quoteId,
        AUDIT_PAGE,
        auditEvents.length,
      );
      auditEvents = [...auditEvents, ...page.events];
      auditTotal = page.total;
    } catch (e) {
      retryError = e instanceof Error ? e.message : String(e);
    } finally {
      auditLoading = false;
    }
  }

  async function onRetry(): Promise<void> {
    if (!quoteId) return;
    retryBusy = true;
    retryError = null;
    try {
      await retryQuotePricingJob(quoteId);
      onRetried();
      await load(quoteId);
    } catch (e) {
      retryError = e instanceof Error ? e.message : String(e);
    } finally {
      retryBusy = false;
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

  // The reason the breakdown section is empty, keyed on the row's
  // state, so the operator sees WHY (not just a blank section).
  function breakdownUnavailableReason(state: string): string {
    if (state === "failed") {
      return "A sor a árazás előtt vagy közben elakadt. / Row failed before or during pricing.";
    }
    return "Az árazás még folyamatban. / Pricing still in progress.";
  }

  function toggleExpand(seq: number) {
    const next = new Set(expandedSeqs);
    if (next.has(seq)) next.delete(seq);
    else next.add(seq);
    expandedSeqs = next;
  }

  function formatPayload(payload: unknown): string {
    try {
      return JSON.stringify(payload, null, 2);
    } catch {
      return String(payload);
    }
  }

  function handleDialogClose() {
    onClose();
  }

  function handleDialogClick(e: MouseEvent) {
    if (e.target === dialogEl) {
      dialogEl?.close();
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="qjd"
  onclose={handleDialogClose}
  onclick={handleDialogClick}
  aria-label="Auto-quoting job detail / Auto-árazás részletek"
  data-testid="pricing-job-detail-dialog"
>
  <div class="qjd__panel">
    {#if loadState === "loading"}
      <p class="qjd__hint">Betöltés… / Loading…</p>
    {:else if loadState === "error"}
      <header class="qjd__hdr">
        <h3>Hiba / Error</h3>
        <button
          type="button"
          class="qjd__close"
          onclick={() => dialogEl?.close()}
          aria-label="Bezárás / Close">✕</button
        >
      </header>
      <p class="qjd__err" data-testid="pricing-job-detail-error">
        {errorMessage ?? "Hiba / Error"}
      </p>
    {:else if detail}
      <!-- Header: Ref + customer + state badge + Refresh + Close -->
      <header class="qjd__hdr">
        <div>
          <h3>
            <code title={detail.quote_id}>{detail.quote_id}</code>
          </h3>
          <div class="qjd__hdr-sub">
            {detail.customer_name}
            <span class={stateChipClass(detail.state)}>
              {stateLabel(detail.state)}
            </span>
            {#if detail.attempt_n > 0}
              <span class="qjd__muted">×{detail.attempt_n}</span>
            {/if}
          </div>
        </div>
        <div class="qjd__hdr-actions">
          <button
            type="button"
            class="btn btn--secondary"
            onclick={() => quoteId && void load(quoteId)}
            data-testid="pricing-job-detail-refresh">Frissítés / Reload</button
          >
          <button
            type="button"
            class="qjd__close"
            onclick={() => dialogEl?.close()}
            aria-label="Bezárás / Close"
            data-testid="pricing-job-detail-close">✕</button
          >
        </div>
      </header>

      {#if retryError}
        <p class="qjd__err" data-testid="pricing-job-detail-retry-error">
          {retryError}
        </p>
      {/if}

      <div class="qjd__body">
        <!-- Submission -->
        <section class="qjd__sec">
          <h4>Beküldés / Submission</h4>
          <dl class="qjd__dl">
            <dt>Vevő / Customer</dt>
            <dd>{detail.customer_name}</dd>
            <dt>E-mail</dt>
            <dd>
              {#if detail.customer_email}
                <a href={`mailto:${detail.customer_email}`}
                  >{detail.customer_email}</a
                >
              {:else}
                —
              {/if}
            </dd>
            <dt>Beérkezett / Fetched</dt>
            <dd>{formatInvoiceDate(detail.fetched_at)}</dd>
            <dt>Frissítve / Updated</dt>
            <dd>{formatInvoiceDate(detail.updated_at)}</dd>
            <dt>Ref</dt>
            <dd><code>{detail.quote_id}</code></dd>
          </dl>
        </section>

        <!-- CAD + Material + Qty -->
        <section class="qjd__sec">
          <h4>CAD &amp; anyag / CAD &amp; material</h4>
          <dl class="qjd__dl">
            <dt>CAD fájl / file</dt>
            <dd>{detail.cad_filename}</dd>
            <dt>Anyag / Material</dt>
            <dd>{detail.material_grade}</dd>
            <dt>Db / Qty</dt>
            <dd>{detail.quantity}</dd>
            <dt>Pénznem / Currency</dt>
            <dd>{detail.currency}</dd>
            <dt>PDF</dt>
            <dd>
              {#if detail.pdf_available}
                <span class="chip chip--ok">Elkészült / Rendered</span>
              {:else}
                <span class="qjd__muted">— nincs / none</span>
              {/if}
            </dd>
            {#if detail.valid_until_iso}
              <dt>Érvényes / Valid until</dt>
              <dd>{detail.valid_until_iso}</dd>
            {/if}
          </dl>
        </section>

        <!-- Pricing breakdown -->
        <section class="qjd__sec">
          <h4>Árazási bontás / Pricing breakdown</h4>
          {#if priceRows.length > 0}
            <table class="qjd__tbl" data-testid="pricing-job-detail-breakdown">
              <tbody>
                {#each priceRows as row (row.label)}
                  <tr>
                    <td>{row.label}</td>
                    <td class="qjd__num">{row.value.toFixed(2)} EUR</td>
                  </tr>
                {/each}
              </tbody>
            </table>
            {#if detail.breakdown?.reasoning_log && detail.breakdown.reasoning_log.length > 0}
              <details class="qjd__details">
                <summary>Indoklás / Reasoning log</summary>
                <ol class="qjd__log">
                  {#each detail.breakdown.reasoning_log as line, i (i)}
                    <li>{line}</li>
                  {/each}
                </ol>
              </details>
            {/if}
          {:else}
            <p
              class="qjd__muted"
              data-testid="pricing-job-detail-breakdown-empty"
            >
              Árazási bontás nem érhető el. / Pricing breakdown not available.
              <br />
              {breakdownUnavailableReason(detail.state)}
            </p>
          {/if}
        </section>

        <!-- FeatureGraph -->
        <section class="qjd__sec">
          <h4>Geometria / FeatureGraph</h4>
          {#if detail.feature_graph}
            {@const fg = detail.feature_graph}
            <dl class="qjd__dl">
              {#if typeof fg.volume_mm3 === "number"}
                <dt>Térfogat / Volume</dt>
                <dd>{fg.volume_mm3.toFixed(1)} mm³</dd>
              {/if}
              {#if fg.bounding_box_mm}
                <dt>Befoglaló / Bounding box</dt>
                <dd>{fg.bounding_box_mm.map((n) => n.toFixed(1)).join(" × ")} mm</dd>
              {/if}
              {#if fg.requires_5_axis !== undefined}
                <dt>5-tengelyes / 5-axis</dt>
                <dd>{fg.requires_5_axis ? "igen / yes" : "nem / no"}</dd>
              {/if}
              {#if fg.thin_wall_present !== undefined}
                <dt>Vékony fal / Thin wall</dt>
                <dd>{fg.thin_wall_present ? "igen / yes" : "nem / no"}</dd>
              {/if}
            </dl>
            {#if fg.features && fg.features.length > 0}
              <table
                class="qjd__tbl"
                data-testid="pricing-job-detail-features"
              >
                <thead>
                  <tr>
                    <th>Jellemző / Feature</th>
                    <th>Db / Count</th>
                    <th>Méret / Size (mm)</th>
                  </tr>
                </thead>
                <tbody>
                  {#each fg.features as f, i (i)}
                    <tr>
                      <td>{f.feature_type}</td>
                      <td>{f.count}</td>
                      <td>{f.representative_size_mm}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            {:else}
              <p class="qjd__muted">Nincs jellemző. / No features detected.</p>
            {/if}
          {:else}
            <p class="qjd__muted">
              Geometria nem érhető el (még nincs CAD-elemzés). / Not available
              (extraction not completed).
            </p>
          {/if}
        </section>

        <!-- Status timeline -->
        <section class="qjd__sec">
          <h4>Állapot-idővonal / Status timeline</h4>
          {#if timeline.length > 0}
            <ol class="qjd__timeline" data-testid="pricing-job-detail-timeline">
              {#each timeline as node, i (i)}
                <li>
                  <span class="qjd__tl-time">{formatInvoiceDate(node.occurred_at)}</span>
                  <span class="qjd__tl-label">{node.label}</span>
                  <span class="qjd__muted"
                    >{node.actor === "system" ? "auto / rendszer" : node.actor}</span
                  >
                </li>
              {/each}
            </ol>
          {:else}
            <p class="qjd__muted">Nincs idővonal-esemény. / No timeline events.</p>
          {/if}
        </section>

        <!-- Last writeback outcome -->
        {#if writeback}
          {@const badge = writebackOutcomeBadge(writeback.outcome)}
          <section class="qjd__sec">
            <h4>Utolsó visszaküldés / Last writeback outcome</h4>
            <div class="qjd__wb-head">
              <span
                class={badge.className}
                data-testid="pricing-job-detail-writeback"
                data-outcome={writeback.outcome}>{badge.label}</span
              >
              {#if writeback.retryable}
                <span class="qjd__muted">↻ újrapróbálható / retryable</span>
              {/if}
            </div>
            <dl class="qjd__dl">
              {#if writeback.http_status !== null}
                <dt>HTTP</dt>
                <dd>{writeback.http_status}</dd>
              {/if}
              {#if writeback.content_type !== null}
                <dt>Content-Type</dt>
                <dd><code>{writeback.content_type}</code></dd>
              {/if}
              {#if writeback.attempt_n !== null}
                <dt>Kísérlet / Attempt</dt>
                <dd>×{writeback.attempt_n}</dd>
              {/if}
              <dt>Időpont / At</dt>
              <dd>{formatInvoiceDate(writeback.occurred_at)}</dd>
            </dl>
            {#if writeback.body_excerpt}
              <details class="qjd__details">
                <summary>Válasz-részlet / Body excerpt</summary>
                <pre class="qjd__pre">{writeback.body_excerpt}</pre>
              </details>
            {/if}
          </section>
        {/if}

        <!-- Error (Failed rows) -->
        {#if detail.state === "failed"}
          <section class="qjd__sec">
            <h4>Hiba / Error</h4>
            {#if detail.error_stage}
              <div class="qjd__err-stage">{detail.error_stage}</div>
            {/if}
            {#if failureKindBadge(detail.failure_kind, detail.error_reason)}
              {@const fb = failureKindBadge(
                detail.failure_kind,
                detail.error_reason,
              )!}
              <div class="qjd__wb-head">
                <span class={fb.className}>{fb.label}</span>
              </div>
            {/if}
            {#if detail.error_reason}
              <pre class="qjd__pre">{detail.error_reason}</pre>
            {/if}
          </section>
        {/if}

        <!-- Audit events (paginated) -->
        <section class="qjd__sec">
          <h4>
            Audit-események / Audit events
            <span class="qjd__muted"
              >({auditEvents.length} / {auditTotal})</span
            >
          </h4>
          {#if auditEvents.length > 0}
            <ul class="qjd__audit" data-testid="pricing-job-detail-audit">
              {#each auditEvents as ev (ev.seq)}
                <li>
                  <button
                    type="button"
                    class="qjd__audit-row"
                    onclick={() => toggleExpand(ev.seq)}
                  >
                    <span class="qjd__tl-time"
                      >{formatInvoiceDate(ev.occurred_at)}</span
                    >
                    <span class="qjd__tl-label">{auditKindLabel(ev.kind)}</span>
                    <span class="qjd__muted"
                      >{ev.actor === "system" ? "auto" : ev.actor}</span
                    >
                  </button>
                  {#if expandedSeqs.has(ev.seq)}
                    <pre class="qjd__pre">{formatPayload(ev.payload)}</pre>
                  {/if}
                </li>
              {/each}
            </ul>
            {#if auditEvents.length < auditTotal}
              <button
                type="button"
                class="btn btn--secondary"
                onclick={() => void loadMoreAudit()}
                disabled={auditLoading}
                data-testid="pricing-job-detail-audit-more"
                >{auditLoading
                  ? "Betöltés… / Loading…"
                  : "Több / Load more"}</button
              >
            {/if}
          {:else}
            <p class="qjd__muted">Nincs audit-esemény. / No audit events.</p>
          {/if}
        </section>
      </div>

      <!-- Footer: Retry (Failed rows only) -->
      {#if detail.state === "failed"}
        <footer class="qjd__footer">
          <button
            type="button"
            class="btn btn--secondary"
            onclick={() => void onRetry()}
            disabled={retryBusy}
            data-testid="pricing-job-detail-retry"
            >{retryBusy
              ? "Újrapróbálás… / Retrying…"
              : "Újra / Retry"}</button
          >
        </footer>
      {/if}
    {/if}
  </div>
</dialog>

<style>
  .qjd {
    border: none;
    padding: 0;
    background: transparent;
    max-width: 760px;
    width: 92vw;
    color: var(--color-text, #e5e7eb);
  }
  .qjd::backdrop {
    background: rgba(0, 0, 0, 0.55);
  }
  .qjd__panel {
    background: var(--color-surface, #1f2937);
    border: 1px solid var(--color-border, #374151);
    border-radius: 8px;
    max-height: 88vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .qjd__hdr {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 16px;
    padding: 16px;
    border-bottom: 1px solid var(--color-border, #374151);
  }
  .qjd__hdr h3 {
    margin: 0;
    font-size: 15px;
  }
  .qjd__hdr-sub {
    margin-top: 6px;
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 13px;
  }
  .qjd__hdr-actions {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .qjd__close {
    background: transparent;
    border: 1px solid var(--color-border, #374151);
    color: var(--color-text, #e5e7eb);
    border-radius: 6px;
    width: 32px;
    height: 32px;
    cursor: pointer;
    font-size: 14px;
  }
  .qjd__body {
    padding: 16px;
    overflow-y: auto;
  }
  .qjd__footer {
    padding: 12px 16px;
    border-top: 1px solid var(--color-border, #374151);
    display: flex;
    justify-content: flex-end;
  }
  .qjd__sec {
    margin-bottom: 18px;
  }
  .qjd__sec h4 {
    margin: 0 0 8px;
    font-size: 13px;
    color: var(--color-text-muted, #9ca3af);
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  .qjd__dl {
    display: grid;
    grid-template-columns: minmax(120px, 30%) 1fr;
    gap: 4px 12px;
    margin: 0;
    font-size: 13px;
  }
  .qjd__dl dt {
    color: var(--color-text-muted, #9ca3af);
  }
  .qjd__dl dd {
    margin: 0;
  }
  .qjd__tbl {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }
  .qjd__tbl th,
  .qjd__tbl td {
    text-align: left;
    padding: 6px 8px;
    border-bottom: 1px solid var(--color-border, #374151);
  }
  .qjd__tbl th {
    color: var(--color-text-muted, #9ca3af);
    font-weight: 600;
  }
  .qjd__num {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .qjd__muted {
    color: var(--color-text-muted, #9ca3af);
    font-size: 12px;
  }
  .qjd__hint,
  .qjd__err {
    padding: 16px;
  }
  .qjd__err {
    color: var(--color-danger, #f87171);
  }
  .qjd__err-stage {
    font-weight: 600;
    color: var(--color-danger, #f87171);
    margin-bottom: 4px;
  }
  .qjd__timeline,
  .qjd__log {
    margin: 0;
    padding-left: 18px;
    font-size: 13px;
  }
  .qjd__timeline {
    list-style: none;
    padding-left: 0;
  }
  .qjd__timeline li {
    display: flex;
    gap: 10px;
    align-items: baseline;
    padding: 4px 0;
    border-bottom: 1px solid var(--color-border, #374151);
    flex-wrap: wrap;
  }
  .qjd__tl-time {
    color: var(--color-text-muted, #9ca3af);
    font-size: 12px;
    min-width: 130px;
  }
  .qjd__tl-label {
    font-weight: 500;
  }
  .qjd__wb-head {
    display: flex;
    gap: 10px;
    align-items: center;
    margin-bottom: 8px;
    flex-wrap: wrap;
  }
  .qjd__details {
    margin-top: 8px;
    font-size: 13px;
  }
  .qjd__details summary {
    cursor: pointer;
    color: var(--color-text-muted, #9ca3af);
  }
  .qjd__pre {
    white-space: pre-wrap;
    word-break: break-word;
    background: var(--color-bg, #111827);
    border: 1px solid var(--color-border, #374151);
    border-radius: 6px;
    padding: 8px;
    font-size: 12px;
    margin: 6px 0 0;
    max-height: 240px;
    overflow: auto;
  }
  .qjd__audit {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .qjd__audit li {
    border-bottom: 1px solid var(--color-border, #374151);
  }
  .qjd__audit-row {
    display: flex;
    gap: 10px;
    align-items: baseline;
    width: 100%;
    background: transparent;
    border: none;
    color: var(--color-text, #e5e7eb);
    padding: 6px 0;
    cursor: pointer;
    text-align: left;
    flex-wrap: wrap;
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
