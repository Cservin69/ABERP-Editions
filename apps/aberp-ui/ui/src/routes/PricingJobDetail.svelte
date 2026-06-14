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
    acceptQuotePricingJob,
    downloadQuotePricingJobPdf,
    editQuotePricingJobMaterial,
    getQuotePricingJob,
    getQuotePricingJobAudit,
    listQuotingMaterials,
    retryQuotePricingJob,
    AcceptQuoteError,
    MaterialEditError,
    type AuditEntryView,
    type PricingJobDetail,
  } from "../lib/api";
  import {
    ACCEPT_CHANNEL_OPTIONS,
    acceptErrorInlineCopy,
    hasOperatorAccepted,
    isAcceptable,
    validateAcceptForm,
    type AcceptChannel,
  } from "../lib/pricing-operator-accept";
  import {
    runQuotePdfAction,
    type QuotePdfAction,
    type QuotePdfDeps,
  } from "../lib/pricing-job-pdf";
  import { formatInvoiceDate } from "../lib/format";
  import { customerCell } from "../lib/pricing-customer-cell";
  import { failureKindBadge } from "../lib/pricing-failure-kind";
  import { writebackOutcomeBadge } from "../lib/pricing-failure-kind";
  import {
    auditKindLabel,
    breakdownRows,
    latestWritebackOutcome,
    reasoningLogLines,
    timelineNodes,
  } from "../lib/pricing-job-detail";
  import {
    isMaterialEditable,
    materialEditInlineCopy,
    materialOptions,
    type MaterialOption,
  } from "../lib/pricing-material-edit";

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

  // S350 / PR-39 (U5) — operator material-grade override state.
  let editingMaterial = $state(false);
  let materialDraft = $state("");
  let materialOpts = $state<MaterialOption[]>([]);
  let materialOptsLoading = $state(false);
  let materialSaveBusy = $state(false);
  let materialEditError = $state<string | null>(null);
  let materialToast = $state<string | null>(null);

  // S354 / PR-42 (U16) — operator accept-on-behalf state.
  let acceptOpen = $state(false);
  let acceptChannel = $state<AcceptChannel | "">("");
  let acceptNote = $state("");
  let acceptPath = $state("");
  let acceptBusy = $state(false);
  let acceptError = $state<string | null>(null);
  let acceptToast = $state<string | null>(null);

  const AUDIT_PAGE = 50;

  // Derived sections — all read off the one audit page we fetch.
  const timeline = $derived(timelineNodes(auditEvents));
  const writeback = $derived(latestWritebackOutcome(auditEvents));
  const priceRows = $derived(breakdownRows(detail?.breakdown ?? null));
  const reasoningLog = $derived(reasoningLogLines(detail?.breakdown ?? null));
  // The Accept button shows only on a Posted (priced + delivered) row that
  // has not already been operator-accepted (the backend 409 is the safety
  // net; this just hides the affordance once synced).
  const alreadyAccepted = $derived(hasOperatorAccepted(auditEvents));

  // Open on a non-null quoteId; close + reset on null. Guard the
  // double-open InvalidStateError per the InvoiceDetail precedent.
  $effect(() => {
    if (!dialogEl) return;
    if (quoteId !== null) {
      if (!dialogEl.open) dialogEl.showModal();
      expandedSeqs = new Set();
      retryError = null;
      cancelMaterialEdit();
      materialToast = null;
      cancelAccept();
      acceptToast = null;
      void load(quoteId);
    } else {
      // Close the inline PDF viewer first so its `close` handler revokes
      // the blob URL (closing the parent alone would orphan it).
      if (pdfViewerEl?.open) pdfViewerEl.close();
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

  // S352 / PR-41 — View/Download the rendered quote PDF. The browser
  // can't set `<a href>` to an authenticated endpoint (it wouldn't send
  // the Bearer), so we fetch the bytes via the Tauri command seam (which
  // injects the Bearer), wrap them in a `Blob`, and view/download the
  // object URL. Logic lives in `runQuotePdfAction` (pure, unit-tested);
  // the real browser seams are supplied here.
  let pdfBusy = $state(false);
  let pdfError = $state<string | null>(null);

  // S402 — View renders the PDF inline in an `<iframe>` modal. The prior
  // `window.open(blobUrl)` was a silent no-op in the Tauri webview
  // (WKWebView on macOS / WebView2 on Windows block popups), so the
  // operator only ever saw Download work — "View … not working only the
  // download." `pdfViewUrl` holds the blob URL while the viewer modal is
  // open; the modal owns its lifetime and revokes it on close.
  let pdfViewerEl: HTMLDialogElement | null = $state(null);
  let pdfViewUrl = $state<string | null>(null);

  const pdfDeps: QuotePdfDeps = {
    download: (id) => downloadQuotePricingJobPdf(id),
    createObjectURL: (blob) => URL.createObjectURL(blob),
    showInline: (url) => {
      pdfViewUrl = url;
    },
    triggerDownload: (url, filename) => {
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = filename;
      document.body.appendChild(anchor);
      anchor.click();
      document.body.removeChild(anchor);
    },
    // Revoke after a delay so the save dialog has consumed the URL. Used
    // on the Download path only; the View URL is revoked by the viewer
    // modal's `close` handler.
    scheduleRevoke: (url) => {
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
    },
  };

  // Open the inline PDF viewer on a non-null blob URL; close on null.
  // Guards the double-`showModal` InvalidStateError per the same
  // precedent as the parent dialog above.
  $effect(() => {
    if (!pdfViewerEl) return;
    if (pdfViewUrl !== null) {
      if (!pdfViewerEl.open) pdfViewerEl.showModal();
    } else {
      if (pdfViewerEl.open) pdfViewerEl.close();
    }
  });

  // Fires on Esc, backdrop click, and programmatic `.close()`. Revoke the
  // blob URL here (the one place every close path funnels through) so we
  // never leak it and never yank it from a still-open iframe.
  function handlePdfViewerClose() {
    if (pdfViewUrl) URL.revokeObjectURL(pdfViewUrl);
    pdfViewUrl = null;
  }

  function handlePdfViewerClick(e: MouseEvent) {
    if (e.target === pdfViewerEl) pdfViewerEl?.close();
  }

  async function onPdfAction(action: QuotePdfAction): Promise<void> {
    if (!quoteId) return;
    pdfBusy = true;
    pdfError = null;
    try {
      await runQuotePdfAction(pdfDeps, quoteId, action);
    } catch (e) {
      pdfError = e instanceof Error ? e.message : String(e);
    } finally {
      pdfBusy = false;
    }
  }

  // S350 / PR-39 (U5) — open the inline material editor: load the
  // catalogue (same snapshot the storefront /quote dropdown uses) and
  // seed the draft with the current grade.
  async function startMaterialEdit(): Promise<void> {
    if (!detail) return;
    editingMaterial = true;
    materialEditError = null;
    materialToast = null;
    materialDraft = detail.material_grade;
    materialOptsLoading = true;
    try {
      const result = await listQuotingMaterials();
      materialOpts = materialOptions(result.materials);
    } catch (e) {
      materialEditError = e instanceof Error ? e.message : String(e);
    } finally {
      materialOptsLoading = false;
    }
  }

  function cancelMaterialEdit() {
    editingMaterial = false;
    materialEditError = null;
    materialDraft = "";
  }

  async function saveMaterialEdit(): Promise<void> {
    if (!quoteId || !detail) return;
    materialSaveBusy = true;
    materialEditError = null;
    try {
      await editQuotePricingJobMaterial(quoteId, materialDraft);
      editingMaterial = false;
      materialToast =
        "Anyag frissítve, újraárazás… / Material updated, re-pricing…";
      // Pull the parent list (state moved back to Fetched, ×N bumped)
      // and refresh this panel from the detail endpoint.
      onRetried();
      await load(quoteId);
    } catch (e) {
      materialEditError =
        e instanceof MaterialEditError
          ? materialEditInlineCopy(e)
          : e instanceof Error
            ? e.message
            : String(e);
    } finally {
      materialSaveBusy = false;
    }
  }

  // S354 / PR-42 (U16) — open the inline accept-on-behalf form.
  function startAccept(): void {
    acceptOpen = true;
    acceptError = null;
    acceptToast = null;
    acceptChannel = "";
    acceptNote = "";
    acceptPath = "";
  }

  function cancelAccept(): void {
    acceptOpen = false;
    acceptError = null;
    acceptChannel = "";
    acceptNote = "";
    acceptPath = "";
  }

  async function saveAccept(): Promise<void> {
    if (!quoteId || !detail) return;
    const validationError = validateAcceptForm({
      channel: acceptChannel,
      note: acceptNote,
    });
    if (validationError) {
      acceptError = validationError;
      return;
    }
    acceptBusy = true;
    acceptError = null;
    try {
      const path = acceptPath.trim();
      await acceptQuotePricingJob(quoteId, {
        // `channel` is non-"" here (validated above).
        channel: acceptChannel as AcceptChannel,
        note: acceptNote.trim(),
        customer_confirmation_path: path.length > 0 ? path : undefined,
      });
      acceptOpen = false;
      acceptToast =
        "Elfogadás rögzítve. / Acceptance recorded.";
      // Refresh the parent list + this panel (the audit timeline now
      // carries the operator-accept event; the Accept button hides).
      onRetried();
      await load(quoteId);
    } catch (e) {
      acceptError =
        e instanceof AcceptQuoteError
          ? acceptErrorInlineCopy(e)
          : e instanceof Error
            ? e.message
            : String(e);
    } finally {
      acceptBusy = false;
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
      <!-- S401 — resolve the Customer-cell shape once for the branch;
           {@const} must be a direct child of the {:else if} block. -->
      {@const cust = customerCell(
        detail.customer_company,
        detail.customer_name,
        detail.customer_email,
      )}
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

      {#if materialToast}
        <p class="qjd__toast" data-testid="pricing-job-material-toast">
          {materialToast}
        </p>
      {/if}

      {#if acceptToast}
        <p class="qjd__toast" data-testid="pricing-job-accept-toast">
          {acceptToast}
        </p>
      {/if}

      <div class="qjd__body">
        <!-- Submission -->
        <section class="qjd__sec">
          <h4>Beküldés / Submission</h4>
          <dl class="qjd__dl">
            <!-- S401 — company first: it's who the operator is quoting. -->
            <dt>Cég / Company</dt>
            <dd
              class:qjd__company-missing={cust.companyMissing}
              data-testid="pricing-job-detail-company"
            >{cust.company}</dd>
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
            <dd>
              {#if editingMaterial}
                <div class="qjd__matedit" data-testid="pricing-job-material-edit">
                  <select
                    class="qjd__matselect"
                    bind:value={materialDraft}
                    disabled={materialOptsLoading || materialSaveBusy}
                    data-testid="pricing-job-material-select"
                  >
                    {#if materialOptsLoading}
                      <option value={materialDraft}>Betöltés… / Loading…</option>
                    {:else}
                      {#each materialOpts as opt (opt.value)}
                        <option value={opt.value}>{opt.label}</option>
                      {/each}
                    {/if}
                  </select>
                  <div class="qjd__matedit-actions">
                    <button
                      type="button"
                      class="btn btn--secondary"
                      onclick={() => void saveMaterialEdit()}
                      disabled={materialSaveBusy || materialOptsLoading}
                      data-testid="pricing-job-material-save"
                      >{materialSaveBusy
                        ? "Mentés… / Saving…"
                        : "Mentés / Save"}</button
                    >
                    <button
                      type="button"
                      class="qjd__close"
                      onclick={cancelMaterialEdit}
                      disabled={materialSaveBusy}
                      aria-label="Mégse / Cancel"
                      data-testid="pricing-job-material-cancel">✕</button
                    >
                  </div>
                </div>
                {#if materialEditError}
                  <p
                    class="qjd__err qjd__matedit-err"
                    data-testid="pricing-job-material-error"
                  >
                    {materialEditError}
                  </p>
                {/if}
              {:else}
                <span class="qjd__matval">{detail.material_grade}</span>
                {#if isMaterialEditable(detail.state)}
                  <button
                    type="button"
                    class="qjd__matpencil"
                    onclick={() => void startMaterialEdit()}
                    aria-label="Anyag szerkesztése / Edit material"
                    title="Anyag szerkesztése / Edit material"
                    data-testid="pricing-job-material-edit-btn">✎</button
                  >
                {/if}
              {/if}
            </dd>
            <dt>Db / Qty</dt>
            <dd>{detail.quantity}</dd>
            <dt>Pénznem / Currency</dt>
            <dd>{detail.currency}</dd>
            <dt>PDF</dt>
            <dd>
              {#if detail.pdf_available}
                <span class="chip chip--ok">Elkészült / Rendered</span>
                <!-- S352 / PR-41 — View/Download the rendered PDF.
                     Authenticated fetch → blob → view/download (the
                     Bearer can't ride an `<a href>`). S402 — View now
                     renders inline in an `<iframe>` modal (window.open
                     is a no-op in the Tauri webview). -->
                <span class="qjd__pdf-actions">
                  <button
                    type="button"
                    class="btn btn--secondary"
                    disabled={pdfBusy}
                    onclick={() => void onPdfAction("view")}
                    data-testid="pricing-job-pdf-view-btn"
                    >👁 Megnyitás / Open</button
                  >
                  <button
                    type="button"
                    class="btn btn--secondary"
                    disabled={pdfBusy}
                    onclick={() => void onPdfAction("download")}
                    data-testid="pricing-job-pdf-download-btn"
                    >⬇ Letöltés / Download</button
                  >
                </span>
              {:else}
                <span class="qjd__muted">— nincs / none</span>
              {/if}
            </dd>
            {#if pdfError}
              <dt></dt>
              <dd>
                <p class="qjd__err" data-testid="pricing-job-pdf-error">
                  {pdfError}
                </p>
              </dd>
            {/if}
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
            {#if reasoningLog.length > 0}
              <!-- S404: expanded by default — the operator sees the FULL
                   pricing logic, no "N more…" hiding. Still collapsible
                   for density, but every line the engine produced is
                   present (matches the un-truncated PDF). -->
              <details class="qjd__details" open>
                <summary>Indoklás / Reasoning log ({reasoningLog.length})</summary
                >
                <ol class="qjd__log" data-testid="pricing-job-reasoning-log">
                  {#each reasoningLog as line, i (i)}
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

      <!-- Footer: Retry (Failed rows) + Accept on behalf (Posted rows) -->
      {#if detail.state === "failed" || (isAcceptable(detail.state) && !alreadyAccepted)}
        <footer class="qjd__footer">
          {#if detail.state === "failed"}
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
          {/if}

          <!-- S354 / PR-42 (U16) — operator accept-on-behalf. Visible on a
               Posted (priced + delivered) row that has not already been
               operator-accepted. -->
          {#if isAcceptable(detail.state) && !alreadyAccepted}
            {#if !acceptOpen}
              <button
                type="button"
                class="btn btn--secondary"
                onclick={startAccept}
                data-testid="pricing-job-accept-btn"
                >Elfogadás / Accept</button
              >
            {:else}
              <div class="qjd__accept" data-testid="pricing-job-accept-form">
                <label class="qjd__accept-row">
                  <span>Csatorna / Channel</span>
                  <select
                    class="qjd__matselect"
                    bind:value={acceptChannel}
                    disabled={acceptBusy}
                    data-testid="pricing-job-accept-channel"
                  >
                    <option value="" disabled>— Válassz / Pick —</option>
                    {#each ACCEPT_CHANNEL_OPTIONS as opt (opt.value)}
                      <option value={opt.value}>{opt.label}</option>
                    {/each}
                  </select>
                </label>
                <label class="qjd__accept-row">
                  <span>Megjegyzés / Note</span>
                  <textarea
                    class="qjd__accept-note"
                    rows="2"
                    bind:value={acceptNote}
                    disabled={acceptBusy}
                    placeholder="Mit mondott az ügyfél, mikor? / What did the customer say, when?"
                    data-testid="pricing-job-accept-note"
                  ></textarea>
                </label>
                <label class="qjd__accept-row">
                  <span>Visszaigazolás útvonal / Confirmation path (opcionális / optional)</span>
                  <input
                    type="text"
                    class="qjd__matselect"
                    bind:value={acceptPath}
                    disabled={acceptBusy}
                    placeholder="/path/to/confirmation.png"
                    data-testid="pricing-job-accept-path"
                  />
                </label>
                <div class="qjd__matedit-actions">
                  <button
                    type="button"
                    class="btn btn--secondary"
                    onclick={() => void saveAccept()}
                    disabled={acceptBusy}
                    data-testid="pricing-job-accept-save"
                    >{acceptBusy
                      ? "Mentés… / Saving…"
                      : "Elfogadás rögzítése / Record accept"}</button
                  >
                  <button
                    type="button"
                    class="qjd__close"
                    onclick={cancelAccept}
                    disabled={acceptBusy}
                    aria-label="Mégse / Cancel"
                    data-testid="pricing-job-accept-cancel">✕</button
                  >
                </div>
                {#if acceptError}
                  <p class="qjd__err" data-testid="pricing-job-accept-error">
                    {acceptError}
                  </p>
                {/if}
              </div>
            {/if}
          {/if}
        </footer>
      {/if}
    {/if}
  </div>
</dialog>

<!-- S402 — inline PDF viewer. A second top-layer `<dialog>` stacked above
     the detail panel so the operator sees the rendered quote without
     leaving the panel. Esc + backdrop click + the ✕ all fire the native
     `close` event → `handlePdfViewerClose` (revokes the blob URL). -->
<dialog
  bind:this={pdfViewerEl}
  class="qjd-pdf"
  onclose={handlePdfViewerClose}
  onclick={handlePdfViewerClick}
  aria-label="Ajánlat PDF előnézet / Quote PDF preview"
  data-testid="pricing-job-pdf-viewer"
>
  {#if pdfViewUrl}
    <div class="qjd-pdf__panel">
      <header class="qjd-pdf__hdr">
        <span class="qjd-pdf__title">Ajánlat PDF / Quote PDF</span>
        <button
          type="button"
          class="qjd__close"
          onclick={() => pdfViewerEl?.close()}
          aria-label="Bezárás / Close"
          data-testid="pricing-job-pdf-viewer-close">✕</button
        >
      </header>
      <iframe
        class="qjd-pdf__frame"
        src={pdfViewUrl}
        title="Ajánlat PDF / Quote PDF"
        data-testid="pricing-job-pdf-iframe"
      ></iframe>
    </div>
  {/if}
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
  /* S402 — inline PDF viewer. Larger than the detail panel so the A4
     quote is legible; same dark surface tokens as the rest of the file. */
  .qjd-pdf {
    border: none;
    padding: 0;
    background: transparent;
    width: 90vw;
    max-width: 900px;
    height: 90vh;
    color: var(--color-text, #e5e7eb);
  }
  .qjd-pdf::backdrop {
    background: rgba(0, 0, 0, 0.7);
  }
  .qjd-pdf__panel {
    background: var(--color-surface, #1f2937);
    border: 1px solid var(--color-border, #374151);
    border-radius: 8px;
    height: 100%;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .qjd-pdf__hdr {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
    padding: 12px 16px;
    border-bottom: 1px solid var(--color-border, #374151);
  }
  .qjd-pdf__title {
    font-size: 13px;
    color: var(--color-text-muted, #9ca3af);
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  .qjd-pdf__frame {
    flex: 1;
    width: 100%;
    border: none;
    background: #525659; /* PDF viewer chrome reads on a neutral grey */
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
  /* S401 — italic muted placeholder when no company was captured. */
  .qjd__company-missing {
    font-style: italic;
    color: var(--color-text-muted, #9ca3af);
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
  /* S350 / PR-39 (U5) — material inline edit. */
  .qjd__matval {
    margin-right: 6px;
  }
  .qjd__matpencil {
    background: transparent;
    border: 1px solid var(--color-border, #374151);
    color: var(--color-text-muted, #9ca3af);
    border-radius: 6px;
    width: 24px;
    height: 24px;
    line-height: 1;
    cursor: pointer;
    font-size: 12px;
    vertical-align: middle;
  }
  .qjd__matpencil:hover {
    color: var(--color-text, #e5e7eb);
  }
  .qjd__matedit {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  /* S352 / PR-41 — View/Download buttons sit inline next to the
     "Rendered" chip; wrap on narrow panels. */
  .qjd__pdf-actions {
    display: inline-flex;
    gap: 6px;
    margin-left: 8px;
    flex-wrap: wrap;
  }
  .qjd__matselect {
    background: var(--color-bg, #111827);
    color: var(--color-text, #e5e7eb);
    border: 1px solid var(--color-border, #374151);
    border-radius: 6px;
    padding: 4px 6px;
    font-size: 13px;
    max-width: 100%;
  }
  .qjd__matedit-actions {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .qjd__matedit-err {
    padding: 6px 0 0;
    font-size: 12px;
  }
  /* S354 / PR-42 (U16) — inline accept-on-behalf form. Stacks the
     channel select, the note, and the optional path; the action row
     reuses `.qjd__matedit-actions`. */
  .qjd__accept {
    display: flex;
    flex-direction: column;
    gap: 8px;
    width: 100%;
  }
  .qjd__accept-row {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12px;
    color: var(--color-text-muted, #9ca3af);
  }
  .qjd__accept-note {
    background: var(--color-bg, #111827);
    color: var(--color-text, #e5e7eb);
    border: 1px solid var(--color-border, #374151);
    border-radius: 6px;
    padding: 6px 8px;
    font-size: 13px;
    font-family: inherit;
    resize: vertical;
    max-width: 100%;
  }
  .qjd__toast {
    margin: 0;
    padding: 8px 16px;
    color: #bbf7d0;
    background: #064e3b;
    font-size: 13px;
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
