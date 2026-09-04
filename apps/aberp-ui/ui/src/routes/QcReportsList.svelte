<script lang="ts">
  // ADR-0199 — QC/AS9102 FAIR + Certificate-of-Conformance report screen.
  // The FIRST UI over the report layer (backend was live, UI absent).
  //
  // A report is drafted against a work order, and a WO is reportable only
  // once it has a dispatch (a report certifies a delivery). So the screen is
  // a master-detail driven off deliveries:
  //   1. Pick a delivery (dispatch) → its work order's reports load.
  //   2. Draft a report (kind + template) → it lands as `drafted`.
  //   3. Open a report → the frozen header, the AS9102 characteristic
  //      accountability, and every line (measured + explicit not-measured).
  //   4. Issue it → the bytes freeze and a SHA-256 is pinned; Download PDF
  //      renders on demand; Void/Supersede retires it (a voided report no
  //      longer renders — corrections are always a new report).
  //
  // Disposition and every count are computed server-side; this screen only
  // displays them and mirrors the ship rule advisorily.

  import { onMount } from "svelte";
  import {
    listDispatches,
    listPartners,
    listQcReports,
    getQcReport,
    issueQcReport,
    voidQcReport,
    downloadQcReportPdf,
    type Dispatch,
    type QcReport,
    type ReportWithLines,
  } from "../lib/api";
  import {
    reportKindLabel,
    templateLabel,
    stateLabel,
    stateTone,
    dispositionLabel,
    dispositionTone,
    permitsShipment,
    accountabilityLabel,
    toleranceLabel,
    actualLabel,
    canIssue,
    canVoid,
    canRenderPdf,
    qcErrorMessage,
    validateVoidReason,
    type Tone,
  } from "../lib/qc-reports";
  import DraftQcReportModal from "./DraftQcReportModal.svelte";

  let dispatches: Dispatch[] = $state([]);
  let partnerNames: Record<string, string> = $state({});
  let deliveriesState: "loading" | "loaded" | "error" = $state("loading");
  let deliveriesError: string | null = $state(null);

  let selectedDsp: Dispatch | null = $state(null);

  let reports: QcReport[] = $state([]);
  let reportsState: "idle" | "loading" | "loaded" | "error" = $state("idle");
  let reportsError: string | null = $state(null);

  let selected: ReportWithLines | null = $state(null);
  let detailState: "idle" | "loading" | "loaded" | "error" = $state("idle");
  let detailError: string | null = $state(null);

  let draftOpen = $state(false);
  let actionError: string | null = $state(null);
  let issuing = $state(false);

  // Inline void form state (keyed to the open report).
  let voiding = $state(false);
  let voidReason = $state("");
  let voidSupersededBy = $state("");
  let voidFieldError: string | null = $state(null);
  let showVoidForm = $state(false);

  onMount(() => {
    void loadDeliveries();
  });

  async function loadDeliveries() {
    deliveriesState = "loading";
    deliveriesError = null;
    try {
      const [dsp, partners] = await Promise.all([
        listDispatches(),
        listPartners(),
      ]);
      dispatches = dsp;
      const names: Record<string, string> = {};
      for (const p of partners) names[p.id] = p.display_name;
      partnerNames = names;
      deliveriesState = "loaded";
    } catch (err: unknown) {
      deliveriesState = "error";
      deliveriesError = qcErrorMessage(
        err instanceof Error ? err.message : String(err),
      );
    }
  }

  function partnerName(id: string): string {
    return partnerNames[id] ?? id;
  }

  function woLabel(d: Dispatch): string {
    return `${partnerName(d.partner_id)} · ${d.wo_id}`;
  }

  async function selectDelivery(d: Dispatch) {
    selectedDsp = d;
    selected = null;
    detailState = "idle";
    showVoidForm = false;
    await loadReports(d.wo_id);
  }

  async function loadReports(woId: string) {
    reportsState = "loading";
    reportsError = null;
    try {
      const resp = await listQcReports(woId);
      reports = resp.reports;
      reportsState = "loaded";
    } catch (err: unknown) {
      reportsState = "error";
      reportsError = qcErrorMessage(
        err instanceof Error ? err.message : String(err),
      );
    }
  }

  async function openReport(qcrId: string) {
    detailState = "loading";
    detailError = null;
    actionError = null;
    showVoidForm = false;
    try {
      selected = await getQcReport(qcrId);
      detailState = "loaded";
    } catch (err: unknown) {
      detailState = "error";
      detailError = qcErrorMessage(
        err instanceof Error ? err.message : String(err),
      );
    }
  }

  async function refreshAfterAction(qcrId: string) {
    // A lifecycle change (issue/void) touches both the header row in the
    // list and the open detail — reload both from the source of truth.
    if (selectedDsp) await loadReports(selectedDsp.wo_id);
    await openReport(qcrId);
  }

  async function onIssue(qcrId: string) {
    actionError = null;
    issuing = true;
    try {
      await issueQcReport(qcrId);
      await refreshAfterAction(qcrId);
    } catch (err: unknown) {
      actionError = qcErrorMessage(
        err instanceof Error ? err.message : String(err),
      );
    } finally {
      issuing = false;
    }
  }

  function openVoidForm() {
    showVoidForm = true;
    voidReason = "";
    voidSupersededBy = "";
    voidFieldError = null;
    actionError = null;
  }

  async function onVoid(qcrId: string) {
    const errs = validateVoidReason(voidReason);
    if (errs.reason) {
      voidFieldError = errs.reason;
      return;
    }
    voidFieldError = null;
    voiding = true;
    actionError = null;
    try {
      const superseded = voidSupersededBy.trim();
      await voidQcReport(qcrId, {
        reason: voidReason.trim(),
        superseded_by_qcr_id: superseded.length > 0 ? superseded : null,
      });
      showVoidForm = false;
      await refreshAfterAction(qcrId);
    } catch (err: unknown) {
      actionError = qcErrorMessage(
        err instanceof Error ? err.message : String(err),
      );
    } finally {
      voiding = false;
    }
  }

  async function onDownloadPdf(qcrId: string) {
    actionError = null;
    try {
      const blob = await downloadQcReportPdf(qcrId);
      const url = URL.createObjectURL(blob);
      window.open(url, "_blank", "noopener");
      // Revoke after the tab has had a chance to take the bytes.
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
    } catch (err: unknown) {
      actionError = qcErrorMessage(
        err instanceof Error ? err.message : String(err),
      );
    }
  }

  async function onDrafted() {
    draftOpen = false;
    if (selectedDsp) await loadReports(selectedDsp.wo_id);
  }

  const toneClass = (t: Tone): string => `chip chip--${t}`;
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <h2 id="page-title" class="page__title">
      Minőségi jelentések / QC reports
    </h2>
    <p class="page__lede">
      AS9102 First Article (FAIR) and Certificate-of-Conformance records,
      issued against a delivery. Issuing freezes the document and pins a
      SHA-256; an incomplete or rejected report blocks the shipment.
    </p>
  </header>

  {#if deliveriesState === "loading"}
    <p class="page__muted">Loading deliveries…</p>
  {:else if deliveriesState === "error"}
    <div class="page__error" role="alert">
      <strong>Could not load deliveries.</strong>
      <p class="page__error-detail">{deliveriesError}</p>
    </div>
  {:else if dispatches.length === 0}
    <div class="page__empty">
      <p>
        No deliveries yet. A QC report certifies a delivery, so create a
        dispatch for a completed work order first.
      </p>
    </div>
  {:else}
    <div class="layout">
      <!-- Deliveries picker -->
      <div class="col col--deliveries">
        <h3 class="col__title">Deliveries / Kiszállítások</h3>
        <ul class="delivery-list">
          {#each dispatches as d (d.dsp_id)}
            <li>
              <button
                type="button"
                class="delivery"
                class:delivery--active={selectedDsp?.dsp_id === d.dsp_id}
                onclick={() => void selectDelivery(d)}
              >
                <span class="delivery__partner">{partnerName(d.partner_id)}</span>
                <span class="delivery__wo mono">{d.wo_id}</span>
                <span class="delivery__state">{d.state}</span>
              </button>
            </li>
          {/each}
        </ul>
      </div>

      <!-- Reports for the selected delivery + detail -->
      <div class="col col--reports">
        {#if selectedDsp === null}
          <p class="page__muted">Select a delivery to see its QC reports.</p>
        {:else}
          <div class="reports__head">
            <h3 class="col__title">
              Reports · <span class="mono">{woLabel(selectedDsp)}</span>
            </h3>
            <button
              type="button"
              class="page__primary"
              onclick={() => (draftOpen = true)}
            >
              + Draft report
            </button>
          </div>

          {#if reportsState === "loading"}
            <p class="page__muted">Loading reports…</p>
          {:else if reportsState === "error"}
            <div class="page__error" role="alert">
              <strong>Could not load reports.</strong>
              <p class="page__error-detail">{reportsError}</p>
            </div>
          {:else if reports.length === 0}
            <p class="page__muted">
              No reports for this delivery yet. Draft the first.
            </p>
          {:else}
            <table class="reports-table">
              <thead>
                <tr>
                  <th scope="col">Report #</th>
                  <th scope="col">Kind</th>
                  <th scope="col">State</th>
                  <th scope="col">Disposition</th>
                  <th scope="col">Qty</th>
                </tr>
              </thead>
              <tbody>
                {#each reports as r (r.qcr_id)}
                  <tr
                    class="report-row"
                    class:report-row--active={selected?.report.qcr_id === r.qcr_id}
                    onclick={() => void openReport(r.qcr_id)}
                  >
                    <td class="mono">{r.report_number || "— (draft)"}</td>
                    <td>{reportKindLabel(r.report_kind)}</td>
                    <td><span class={toneClass(stateTone(r.state))}>{stateLabel(r.state)}</span></td>
                    <td>
                      <span class={toneClass(dispositionTone(r.disposition))}>
                        {dispositionLabel(r.disposition)}
                      </span>
                    </td>
                    <td class="mono">{r.qty_reported}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          {/if}

          <!-- Detail pane -->
          {#if detailState === "loading"}
            <p class="page__muted">Loading report…</p>
          {:else if detailState === "error"}
            <div class="page__error" role="alert">
              <strong>Could not load the report.</strong>
              <p class="page__error-detail">{detailError}</p>
            </div>
          {:else if selected !== null}
            {@const rep = selected.report}
            <div class="detail">
              <div class="detail__head">
                <div>
                  <h4 class="detail__title">
                    {reportKindLabel(rep.report_kind)}
                    {#if rep.report_number}
                      · <span class="mono">{rep.report_number}</span>
                    {/if}
                  </h4>
                  <div class="detail__chips">
                    <span class={toneClass(stateTone(rep.state))}>{stateLabel(rep.state)}</span>
                    <span class={toneClass(dispositionTone(rep.disposition))}>
                      {dispositionLabel(rep.disposition)}
                    </span>
                    {#if !permitsShipment(rep.disposition)}
                      <span class="chip chip--danger">Blocks shipment</span>
                    {/if}
                  </div>
                </div>
                <div class="detail__actions">
                  {#if canIssue(rep)}
                    <button
                      type="button"
                      class="primary-button"
                      disabled={issuing}
                      onclick={() => void onIssue(rep.qcr_id)}
                    >
                      {issuing ? "Issuing…" : "Issue"}
                    </button>
                  {/if}
                  {#if canRenderPdf(rep)}
                    <button
                      type="button"
                      class="quiet-button"
                      onclick={() => void onDownloadPdf(rep.qcr_id)}
                    >
                      Download PDF
                    </button>
                  {/if}
                  {#if canVoid(rep)}
                    <button
                      type="button"
                      class="quiet-button danger"
                      onclick={openVoidForm}
                    >
                      Void
                    </button>
                  {/if}
                </div>
              </div>

              {#if actionError !== null}
                <p class="detail__error" role="alert">{actionError}</p>
              {/if}

              {#if showVoidForm}
                <div class="void-form">
                  <label class="field">
                    <span class="field__label">Reason (required)</span>
                    <input
                      class="field__input"
                      value={voidReason}
                      oninput={(e) =>
                        (voidReason = (e.currentTarget as HTMLInputElement).value)}
                      placeholder="e.g. superseded by corrected revision"
                    />
                  </label>
                  <label class="field">
                    <span class="field__label">
                      Superseded by report id (optional)
                    </span>
                    <input
                      class="field__input mono"
                      value={voidSupersededBy}
                      oninput={(e) =>
                        (voidSupersededBy = (e.currentTarget as HTMLInputElement).value)}
                      placeholder="qcr_… — leave blank for a plain void"
                    />
                  </label>
                  {#if voidFieldError !== null}
                    <p class="detail__error" role="alert">{voidFieldError}</p>
                  {/if}
                  <div class="void-form__buttons">
                    <button
                      type="button"
                      class="quiet-button"
                      onclick={() => (showVoidForm = false)}
                    >
                      Cancel
                    </button>
                    <button
                      type="button"
                      class="quiet-button danger"
                      disabled={voiding}
                      onclick={() => void onVoid(rep.qcr_id)}
                    >
                      {voiding ? "Voiding…" : "Confirm void"}
                    </button>
                  </div>
                </div>
              {/if}

              <!-- Frozen header snapshot -->
              <dl class="detail__grid">
                <div><dt>Template</dt><dd>{templateLabel(rep.template)}</dd></div>
                <div><dt>Work order</dt><dd class="mono">{rep.wo_id}</dd></div>
                <div><dt>Product</dt><dd class="mono">{rep.product_id}</dd></div>
                <div><dt>Customer</dt><dd>{rep.customer_name ?? "—"}</dd></div>
                <div>
                  <dt>Drawing</dt>
                  <dd class="mono">
                    {rep.drawing_number ?? "—"}{rep.drawing_rev ? ` rev ${rep.drawing_rev}` : ""}
                  </dd>
                </div>
                <div><dt>Heat lot</dt><dd class="mono">{rep.heat_lot_reference ?? "—"}</dd></div>
                <div><dt>Serial range</dt><dd class="mono">{rep.serial_range ?? "—"}</dd></div>
                <div><dt>Qty reported</dt><dd class="mono">{rep.qty_reported}</dd></div>
                {#if rep.issued_at_utc}
                  <div><dt>Issued</dt><dd>{rep.issued_at_utc} · {rep.issued_by ?? "—"}</dd></div>
                {/if}
                {#if rep.rendered_sha256}
                  <div class="detail__grid-wide">
                    <dt>Pinned SHA-256</dt>
                    <dd class="mono sha">{rep.rendered_sha256}</dd>
                  </div>
                {/if}
                {#if rep.superseded_by_qcr_id}
                  <div><dt>Superseded by</dt><dd class="mono">{rep.superseded_by_qcr_id}</dd></div>
                {/if}
              </dl>

              <!-- AS9102 characteristic accountability -->
              <div class="acct">
                <span class="acct__item">
                  <span class="acct__n">{rep.characteristics_required}</span> required
                </span>
                <span class="acct__item">
                  <span class="acct__n">{rep.characteristics_measured}</span> measured
                </span>
                <span class="acct__item acct__item--pass">
                  <span class="acct__n">{rep.characteristics_passed}</span> passed
                </span>
                <span class="acct__item acct__item--fail">
                  <span class="acct__n">{rep.characteristics_failed}</span> failed
                </span>
                <span class="acct__item acct__item--unacct">
                  <span class="acct__n">{rep.characteristics_unaccounted}</span> unaccounted
                </span>
              </div>

              <!-- Frozen lines -->
              <table class="lines-table">
                <thead>
                  <tr>
                    <th scope="col">#</th>
                    <th scope="col">Serial</th>
                    <th scope="col">Char.</th>
                    <th scope="col">Nominal ± tol</th>
                    <th scope="col">Actual</th>
                    <th scope="col">Verdict</th>
                    <th scope="col">Accountability</th>
                  </tr>
                </thead>
                <tbody>
                  {#each selected.lines as ln (ln.qcrl_id)}
                    <tr class:line--unacct={ln.accountability === "not_measured"}>
                      <td class="mono">{ln.line_no}</td>
                      <td class="mono">{ln.part_serial ?? "—"}</td>
                      <td>
                        {#if ln.characteristic_number}
                          <span class="mono">{ln.characteristic_number}</span>
                        {/if}
                        {ln.characteristic_name}
                      </td>
                      <td class="mono">{toleranceLabel(ln)}</td>
                      <td class="mono">{actualLabel(ln)}</td>
                      <td>{ln.verdict ?? "—"}</td>
                      <td>{accountabilityLabel(ln.accountability)}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        {/if}
      </div>
    </div>
  {/if}
</section>

{#if draftOpen && selectedDsp !== null}
  <DraftQcReportModal
    woId={selectedDsp.wo_id}
    woLabel={woLabel(selectedDsp)}
    onSaved={onDrafted}
    onClose={() => (draftOpen = false)}
  />
{/if}

<style>
  .page {
    max-width: 1280px;
    margin: 0 auto;
  }
  .page__head {
    margin-bottom: var(--space-4);
  }
  .page__title {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-lg);
    font-weight: 600;
    color: var(--color-text-strong);
  }
  .page__lede {
    margin: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
    max-width: 70ch;
  }
  .page__muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }
  .page__empty {
    padding: var(--space-5);
    border: 1px dashed var(--color-surface-divider);
    background: var(--color-surface-raised);
    text-align: center;
    color: var(--color-text-secondary);
  }
  .page__error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    font-size: var(--type-size-sm);
  }
  .page__error-detail {
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .page__primary {
    padding: var(--space-2) var(--space-4);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .layout {
    display: grid;
    grid-template-columns: 280px 1fr;
    gap: var(--space-4);
    align-items: start;
  }
  .col__title {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .delivery-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .delivery {
    width: 100%;
    text-align: left;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    border-radius: var(--radius-sm);
    cursor: pointer;
    color: var(--color-text-primary);
  }
  .delivery:hover {
    border-color: var(--color-text-secondary);
  }
  .delivery--active {
    border-color: var(--color-signal-positive, var(--color-text-strong));
    box-shadow: inset 2px 0 0 var(--color-signal-positive, var(--color-text-strong));
  }
  .delivery__partner {
    font-size: var(--type-size-sm);
    color: var(--color-text-strong);
    font-weight: 500;
  }
  .delivery__wo {
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }
  .delivery__state {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .reports__head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-2);
  }

  .reports-table,
  .lines-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }
  .reports-table th,
  .reports-table td,
  .lines-table th,
  .lines-table td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
    color: var(--color-text-primary);
  }
  .reports-table th,
  .lines-table th {
    color: var(--color-text-secondary);
    font-weight: 500;
    background: var(--color-surface-raised);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: var(--type-size-xs);
  }
  .report-row {
    cursor: pointer;
  }
  .report-row:hover td {
    background: var(--color-surface-raised);
  }
  .report-row--active td {
    background: var(--color-surface-raised);
    box-shadow: inset 2px 0 0 var(--color-signal-positive, var(--color-text-strong));
  }
  .mono {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  .chip {
    display: inline-block;
    padding: 0 var(--space-2);
    border-radius: var(--radius-lg, 999px);
    border: 1px solid var(--color-surface-divider);
    font-size: var(--type-size-xs);
    font-weight: 500;
    white-space: nowrap;
  }
  .chip--positive {
    color: var(--color-signal-positive, var(--color-text-strong));
    border-color: var(--color-signal-positive, var(--color-text-strong));
  }
  .chip--warning {
    color: var(--color-signal-caution, #b8860b);
    border-color: var(--color-signal-caution, #b8860b);
  }
  .chip--danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .chip--neutral {
    color: var(--color-text-secondary);
  }

  .detail {
    margin-top: var(--space-4);
    padding: var(--space-4);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-md, 8px);
    background: var(--color-surface-raised);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .detail__head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .detail__title {
    margin: 0 0 var(--space-1) 0;
    font-size: var(--type-size-md, var(--type-size-sm));
    font-weight: 600;
    color: var(--color-text-strong);
  }
  .detail__chips {
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  .detail__actions {
    display: flex;
    gap: var(--space-2);
    flex-shrink: 0;
  }
  .detail__error {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    word-break: break-word;
  }

  .detail__grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
    gap: var(--space-2) var(--space-4);
    margin: 0;
  }
  .detail__grid div {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .detail__grid-wide {
    grid-column: 1 / -1;
  }
  .detail__grid dt {
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--color-text-muted);
  }
  .detail__grid dd {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }
  .sha {
    word-break: break-all;
    font-size: var(--type-size-xs);
  }

  .acct {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .acct__n {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
    font-weight: 600;
  }
  .acct__item--pass .acct__n {
    color: var(--color-signal-positive, var(--color-text-strong));
  }
  .acct__item--fail .acct__n,
  .acct__item--unacct .acct__n {
    color: var(--color-signal-negative);
  }

  .line--unacct td {
    background: color-mix(in srgb, var(--color-signal-negative) 8%, transparent);
  }

  .void-form {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-base);
    max-width: 480px;
  }
  .void-form__buttons {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .field__label {
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
  }
  .field__input {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
    border-radius: var(--radius-sm);
    font-family: var(--type-family-body);
  }

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    border-radius: var(--radius-sm);
  }
  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }
  .quiet-button.danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .primary-button {
    padding: var(--space-1) var(--space-4);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }
  .primary-button:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
