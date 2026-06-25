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
    downloadQuotePricingJobPdf,
    editQuotePricingJobMaterial,
    getQuotePricingJob,
    getQuotePricingJobAudit,
    listQuotingMaterials,
    overrideQuoteLeadTime,
    overrideQuoteMargin,
    setQuoteBuyerPartner,
    setQuoteStockForm,
    retryQuotePricingJob,
    MaterialEditError,
    type AuditEntryView,
    type Partner,
    type PricingJobDetail,
  } from "../lib/api";
  import { formatPercent } from "../lib/margin-profiles";
  import PartnerTypeahead from "../lib/PartnerTypeahead.svelte";
  // S427 — lead-time chip colour + effective-value helper (pure).
  import {
    effectiveLeadTime,
    leadTimeChipClass,
  } from "../lib/machines";
  // S416 — the operator accept-on-behalf form (imports from the removed
  // `../lib/pricing-operator-accept`) is gone: the customer accepts via the
  // Accept button in their quote e-mail, so an operator-side Accept was
  // redundant and bypassed the customer.
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

  // S427 — operator lead-time override state.
  let leadTimeDraft = $state("");
  let leadTimeBusy = $state(false);
  let leadTimeError = $state<string | null>(null);

  // S428 — operator margin override + buyer-partner state.
  let marginDraft = $state(""); // percent string, e.g. "30"
  let marginBusy = $state(false);
  let marginError = $state<string | null>(null);
  let buyerSearch = $state("");
  let buyerBusy = $state(false);
  let buyerError = $state<string | null>(null);
  // Below-floor confirmation modal: holds the pending percent + reason.
  let floorConfirm = $state<{ realizedPct: number; floorPct: number } | null>(
    null,
  );
  let floorReason = $state("");

  // S2 / ADR-0094 Gap 1 — operator stock-form override state. Drafts are
  // strings (parsed on save), matching the margin/lead-time controls.
  let editingStockForm = $state(false);
  let stockKindDraft = $state<"rectangular_block" | "round_bar" | "tube">(
    "rectangular_block",
  );
  let stockOdDraft = $state(""); // diameter (round bar) or OD (tube), mm
  let stockIdDraft = $state(""); // bore (tube only), mm
  let stockLenDraft = $state(""); // length, mm
  let stockBusy = $state(false);
  let stockError = $state<string | null>(null);
  let stockToast = $state<string | null>(null);

  const AUDIT_PAGE = 50;

  // Derived sections — all read off the one audit page we fetch.
  const timeline = $derived(timelineNodes(auditEvents));
  const writeback = $derived(latestWritebackOutcome(auditEvents));
  const priceRows = $derived(breakdownRows(detail?.breakdown ?? null));
  const reasoningLog = $derived(reasoningLogLines(detail?.breakdown ?? null));

  // S427 — effective lead-time: override wins over engine-computed.
  const effLeadTime = $derived(
    effectiveLeadTime(
      detail?.lead_time_days ?? null,
      detail?.lead_time_override_days ?? null,
    ),
  );

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
      // S427 — seed the override input with the current override (blank
      // when none is set, so the operator types a fresh value).
      leadTimeDraft =
        d.lead_time_override_days !== null
          ? String(d.lead_time_override_days)
          : "";
      leadTimeError = null;
      // S428 — seed the margin override input (as a percent string) +
      // reset the per-quote margin control state.
      marginDraft =
        d.margin_override_pct !== null ? String(d.margin_override_pct * 100) : "";
      marginError = null;
      buyerError = null;
      buyerSearch = "";
      floorConfirm = null;
      floorReason = "";
      // S2 — seed the stock-form control from the persisted columns.
      seedStockDrafts(d);
      stockError = null;
      stockToast = null;
      editingStockForm = false;
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

  // ── S2 / ADR-0094 Gap 1 — operator stock-form intake ────────────────
  // Humanise the persisted columns for the read-only display.
  function stockFormLabel(d: PricingJobDetail): string {
    if (d.stock_form === "round_bar") {
      return `Rúd / Round bar — Ø${d.stock_od_mm ?? "?"} × ${d.stock_length_mm ?? "?"} mm`;
    }
    if (d.stock_form === "tube") {
      return `Cső / Tube — Ø${d.stock_od_mm ?? "?"}/${d.stock_id_mm ?? "?"} × ${d.stock_length_mm ?? "?"} mm`;
    }
    return "Tömb / Rectangular block (alapért. / default)";
  }

  function seedStockDrafts(d: PricingJobDetail | null): void {
    stockKindDraft =
      d?.stock_form === "round_bar" || d?.stock_form === "tube"
        ? d.stock_form
        : "rectangular_block";
    stockOdDraft = d?.stock_od_mm != null ? String(d.stock_od_mm) : "";
    stockIdDraft = d?.stock_id_mm != null ? String(d.stock_id_mm) : "";
    stockLenDraft = d?.stock_length_mm != null ? String(d.stock_length_mm) : "";
  }

  function startStockFormEdit(): void {
    if (!detail) return;
    editingStockForm = true;
    stockError = null;
    stockToast = null;
  }

  function cancelStockFormEdit(): void {
    editingStockForm = false;
    stockError = null;
    seedStockDrafts(detail); // discard unsaved edits
  }

  async function saveStockForm(): Promise<void> {
    if (!quoteId) return;
    stockBusy = true;
    stockError = null;
    try {
      let body: {
        kind: "round_bar" | "tube" | null;
        od_mm: number | null;
        id_mm: number | null;
        length_mm: number | null;
      };
      if (stockKindDraft === "round_bar") {
        const od = Number(stockOdDraft);
        const len = Number(stockLenDraft);
        if (!(od > 0) || !(len > 0)) {
          stockError =
            "Rúd: átmérő és hossz > 0 / Round bar: diameter and length > 0";
          return;
        }
        body = { kind: "round_bar", od_mm: od, id_mm: null, length_mm: len };
      } else if (stockKindDraft === "tube") {
        const od = Number(stockOdDraft);
        const id = Number(stockIdDraft);
        const len = Number(stockLenDraft);
        if (!(od > 0) || !(id > 0) || !(len > 0)) {
          stockError = "Cső: KÁ, BÁ és hossz > 0 / Tube: OD, ID and length > 0";
          return;
        }
        if (id >= od) {
          stockError =
            "Cső: a furat (BÁ) legyen kisebb a külső átmérőnél / Tube: bore < OD";
          return;
        }
        body = { kind: "tube", od_mm: od, id_mm: id, length_mm: len };
      } else {
        body = { kind: null, od_mm: null, id_mm: null, length_mm: null };
      }
      await setQuoteStockForm(quoteId, body);
      editingStockForm = false;
      stockToast =
        "Nyersanyag-forma frissítve, újraárazás… / Stock form updated, re-pricing…";
      await load(quoteId);
    } catch (e) {
      stockError = e instanceof Error ? e.message : String(e);
    } finally {
      stockBusy = false;
    }
  }

  // S427 — save the operator lead-time override. An empty draft is
  // refused inline (use "Clear override" to remove an override); a
  // non-numeric value is likewise refused before the round-trip.
  async function saveLeadTimeOverride(): Promise<void> {
    if (!quoteId) return;
    const trimmed = leadTimeDraft.trim();
    if (trimmed.length === 0) {
      leadTimeError =
        "Adjon meg egy napszámot, vagy törölje a felülírást. / Enter a day count, or clear the override.";
      return;
    }
    const days = Number(trimmed);
    if (!Number.isFinite(days) || days < 0) {
      leadTimeError =
        "Érvénytelen napszám. / Invalid day count.";
      return;
    }
    leadTimeBusy = true;
    leadTimeError = null;
    try {
      await overrideQuoteLeadTime(quoteId, days);
      await load(quoteId);
    } catch (e) {
      leadTimeError = e instanceof Error ? e.message : String(e);
    } finally {
      leadTimeBusy = false;
    }
  }

  // S427 — clear the override (sends null); effective lead-time falls
  // back to the engine-computed value.
  async function clearLeadTimeOverride(): Promise<void> {
    if (!quoteId) return;
    leadTimeBusy = true;
    leadTimeError = null;
    try {
      await overrideQuoteLeadTime(quoteId, null);
      await load(quoteId);
    } catch (e) {
      leadTimeError = e instanceof Error ? e.message : String(e);
    } finally {
      leadTimeBusy = false;
    }
  }

  // S428 — assign a buyer partner (drives the margin profile) + re-price.
  async function pickBuyerPartner(partner: Partner): Promise<void> {
    if (!quoteId) return;
    buyerBusy = true;
    buyerError = null;
    try {
      await setQuoteBuyerPartner(quoteId, partner.id);
      buyerSearch = "";
      await load(quoteId);
    } catch (e) {
      buyerError = e instanceof Error ? e.message : String(e);
    } finally {
      buyerBusy = false;
    }
  }

  async function clearBuyerPartner(): Promise<void> {
    if (!quoteId) return;
    buyerBusy = true;
    buyerError = null;
    try {
      await setQuoteBuyerPartner(quoteId, null);
      await load(quoteId);
    } catch (e) {
      buyerError = e instanceof Error ? e.message : String(e);
    } finally {
      buyerBusy = false;
    }
  }

  // S428 — extract the `below_margin_floor` 409 payload (realized + floor)
  // from the Tauri-wrapped error string so the confirm modal can show the
  // numbers. Returns null for any other error shape.
  function parseFloorConflict(
    raw: string,
  ): { realizedPct: number; floorPct: number } | null {
    const start = raw.indexOf("{");
    const end = raw.lastIndexOf("}");
    if (start < 0 || end <= start) return null;
    try {
      const obj = JSON.parse(raw.slice(start, end + 1)) as Record<string, unknown>;
      if (obj.error !== "below_margin_floor") return null;
      const realized = obj.realized_pct;
      const floor = obj.floor_pct;
      if (typeof realized !== "number" || typeof floor !== "number") return null;
      return { realizedPct: realized, floorPct: floor };
    } catch {
      return null;
    }
  }

  // S428 — apply the margin override. `confirm` proceeds past a below-floor
  // guard (with the operator's reason). On an unconfirmed below-floor 409,
  // surfaces the confirm modal instead of an error.
  async function applyMarginOverride(
    pct: number | null,
    confirm: boolean,
  ): Promise<void> {
    if (!quoteId) return;
    marginBusy = true;
    marginError = null;
    try {
      await overrideQuoteMargin(
        quoteId,
        pct,
        confirm,
        confirm ? floorReason.trim() : null,
      );
      floorConfirm = null;
      floorReason = "";
      await load(quoteId);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      const conflict = parseFloorConflict(msg);
      if (conflict !== null && !confirm) {
        floorConfirm = conflict;
      } else {
        marginError = msg;
      }
    } finally {
      marginBusy = false;
    }
  }

  function saveMarginOverride(): void {
    const trimmed = marginDraft.trim();
    if (trimmed.length === 0) {
      marginError = "Adj meg egy árrés százalékot. / Enter a margin percent.";
      return;
    }
    const pct = Number(trimmed);
    if (!Number.isFinite(pct) || pct < 0) {
      marginError = "Érvénytelen árrés. / Invalid margin percent.";
      return;
    }
    void applyMarginOverride(pct / 100, false);
  }

  function clearMarginOverride(): void {
    void applyMarginOverride(null, false);
  }

  function confirmFloorOverride(): void {
    const trimmed = marginDraft.trim();
    const pct = Number(trimmed);
    if (!Number.isFinite(pct)) return;
    void applyMarginOverride(pct / 100, true);
  }

  function cancelFloorOverride(): void {
    floorConfirm = null;
    floorReason = "";
  }

  // S416 — the inline operator accept-on-behalf form (startAccept /
  // cancelAccept / saveAccept) was removed; the customer accepts via the
  // Accept button in their quote e-mail.

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
            <!-- S427 — lead-time chip (effective = override ?? computed),
                 coloured ok/warning/err by day count. -->
            {#if effLeadTime !== null}
              <span
                class={leadTimeChipClass(effLeadTime)}
                data-testid="pricing-job-lead-time-chip"
                >Lead time: {effLeadTime} days</span
              >
            {:else}
              <span class="chip" data-testid="pricing-job-lead-time-chip"
                >Lead time: —</span
              >
            {/if}
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
            <dt>Nyersanyag-forma / Stock form</dt>
            <dd>
              {#if editingStockForm}
                <div
                  class="qjd__matedit qjd__stockedit"
                  data-testid="pricing-job-stock-edit"
                >
                  <select
                    class="qjd__matselect"
                    bind:value={stockKindDraft}
                    disabled={stockBusy}
                    data-testid="pricing-job-stock-kind"
                  >
                    <option value="rectangular_block"
                      >Tömb / Rectangular block</option
                    >
                    <option value="round_bar">Rúd / Round bar</option>
                    <option value="tube">Cső / Tube</option>
                  </select>
                  {#if stockKindDraft === "round_bar"}
                    <div class="qjd__stockdims">
                      <label class="qjd__stockfield">
                        <span>Ø átmérő / dia (mm)</span>
                        <input
                          class="qjd__stockinput"
                          type="text"
                          inputmode="decimal"
                          bind:value={stockOdDraft}
                          disabled={stockBusy}
                          data-testid="pricing-job-stock-od"
                        />
                      </label>
                      <label class="qjd__stockfield">
                        <span>Hossz / length (mm)</span>
                        <input
                          class="qjd__stockinput"
                          type="text"
                          inputmode="decimal"
                          bind:value={stockLenDraft}
                          disabled={stockBusy}
                          data-testid="pricing-job-stock-len"
                        />
                      </label>
                    </div>
                  {:else if stockKindDraft === "tube"}
                    <div class="qjd__stockdims">
                      <label class="qjd__stockfield">
                        <span>Ø külső / OD (mm)</span>
                        <input
                          class="qjd__stockinput"
                          type="text"
                          inputmode="decimal"
                          bind:value={stockOdDraft}
                          disabled={stockBusy}
                          data-testid="pricing-job-stock-od"
                        />
                      </label>
                      <label class="qjd__stockfield">
                        <span>Ø furat / ID (mm)</span>
                        <input
                          class="qjd__stockinput"
                          type="text"
                          inputmode="decimal"
                          bind:value={stockIdDraft}
                          disabled={stockBusy}
                          data-testid="pricing-job-stock-id"
                        />
                      </label>
                      <label class="qjd__stockfield">
                        <span>Hossz / length (mm)</span>
                        <input
                          class="qjd__stockinput"
                          type="text"
                          inputmode="decimal"
                          bind:value={stockLenDraft}
                          disabled={stockBusy}
                          data-testid="pricing-job-stock-len"
                        />
                      </label>
                    </div>
                  {/if}
                  <div class="qjd__matedit-actions">
                    <button
                      type="button"
                      class="btn btn--secondary"
                      onclick={() => void saveStockForm()}
                      disabled={stockBusy}
                      data-testid="pricing-job-stock-save"
                      >{stockBusy
                        ? "Mentés… / Saving…"
                        : "Mentés / Save"}</button
                    >
                    <button
                      type="button"
                      class="qjd__close"
                      onclick={cancelStockFormEdit}
                      disabled={stockBusy}
                      aria-label="Mégse / Cancel"
                      data-testid="pricing-job-stock-cancel">✕</button
                    >
                  </div>
                </div>
                {#if stockError}
                  <p
                    class="qjd__err qjd__matedit-err"
                    data-testid="pricing-job-stock-error"
                  >
                    {stockError}
                  </p>
                {/if}
              {:else}
                <span class="qjd__matval" data-testid="pricing-job-stock-value"
                  >{stockFormLabel(detail)}</span
                >
                <button
                  type="button"
                  class="qjd__matpencil"
                  onclick={startStockFormEdit}
                  aria-label="Nyersanyag-forma szerkesztése / Edit stock form"
                  title="Nyersanyag-forma szerkesztése / Edit stock form"
                  data-testid="pricing-job-stock-edit-btn">✎</button
                >
              {/if}
              {#if stockToast}
                <p class="qjd__toast" data-testid="pricing-job-stock-toast">
                  {stockToast}
                </p>
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

        <!-- S427 — Lead time + operator override. -->
        <section class="qjd__sec">
          <h4>Átfutási idő / Lead time</h4>
          <dl class="qjd__dl">
            <dt>Motor / Engine</dt>
            <dd>
              {#if detail.lead_time_days !== null}
                {detail.lead_time_days} nap / days
              {:else}
                — nincs / none
              {/if}
            </dd>
            <dt>Felülírás / Override</dt>
            <dd>
              {#if detail.lead_time_override_days !== null}
                {detail.lead_time_override_days} nap / days
              {:else}
                — nincs / none
              {/if}
            </dd>
            <dt>Tényleges / Effective</dt>
            <dd data-testid="pricing-job-lead-time-effective">
              {#if effLeadTime !== null}
                {effLeadTime} nap / days
              {:else}
                —
              {/if}
            </dd>
          </dl>
          <div class="qjd__leadtime-edit" data-testid="pricing-job-lead-time-edit">
            <input
              type="number"
              min="0"
              step="1"
              class="qjd__matselect"
              bind:value={leadTimeDraft}
              disabled={leadTimeBusy}
              placeholder="napok / days"
              aria-label="Lead-time override in days"
              data-testid="pricing-job-lead-time-input"
            />
            <button
              type="button"
              class="btn btn--secondary"
              onclick={() => void saveLeadTimeOverride()}
              disabled={leadTimeBusy}
              data-testid="pricing-job-lead-time-save"
              >{leadTimeBusy ? "Mentés… / Saving…" : "Mentés / Save"}</button
            >
            <button
              type="button"
              class="btn btn--secondary"
              onclick={() => void clearLeadTimeOverride()}
              disabled={leadTimeBusy || detail.lead_time_override_days === null}
              data-testid="pricing-job-lead-time-clear"
              >Felülírás törlése / Clear override</button
            >
          </div>
          {#if leadTimeError}
            <p class="qjd__err qjd__matedit-err" data-testid="pricing-job-lead-time-error">
              {leadTimeError}
            </p>
          {/if}
        </section>

        <!-- S428 — Margin profile + operator override. -->
        <section class="qjd__sec">
          <h4>Árrés / Margin</h4>

          {#if detail.margin_below_floor}
            <div
              class="qjd__floor-banner"
              role="alert"
              data-testid="pricing-job-margin-floor-banner"
            >
              <strong>⚠ Árrés a minimum alatt / Margin below floor.</strong>
              {#if detail.margin_floor_pct !== null}
                A minimum árrés {formatPercent(detail.margin_floor_pct)}; ezt a
                quote-ot nem lehet DEAL-elni. / The minimum margin is
                {formatPercent(detail.margin_floor_pct)}; this quote cannot be
                dealt.
              {:else}
                Ezt a quote-ot nem lehet DEAL-elni. / This quote cannot be dealt.
              {/if}
            </div>
          {/if}

          <dl class="qjd__dl">
            <dt>Vevő partner / Buyer partner</dt>
            <dd>
              {#if detail.buyer_partner_id !== null}
                <code>{detail.buyer_partner_id}</code>
              {:else}
                — nincs hozzárendelve / unassigned (global default)
              {/if}
            </dd>
            <dt>Felülírás / Override</dt>
            <dd>
              {#if detail.margin_override_pct !== null}
                {formatPercent(detail.margin_override_pct)}
              {:else}
                — nincs / none
              {/if}
            </dd>
            <dt>Minimum / Floor</dt>
            <dd>
              {#if detail.margin_floor_pct !== null}
                {formatPercent(detail.margin_floor_pct)}
              {:else}
                — globális / global
              {/if}
            </dd>
          </dl>

          <div class="qjd__buyer-edit">
            <span class="qjd__field-label">
              Vevő hozzárendelése / Assign buyer
            </span>
            <PartnerTypeahead
              bind:value={buyerSearch}
              onSelect={(p) => void pickBuyerPartner(p)}
              placeholder="Partner neve… / Buyer name…"
              ariaLabel="Assign buyer partner"
            />
            <button
              type="button"
              class="btn btn--secondary"
              onclick={() => void clearBuyerPartner()}
              disabled={buyerBusy || detail.buyer_partner_id === null}
              data-testid="pricing-job-buyer-clear"
              >Hozzárendelés törlése / Clear buyer</button
            >
          </div>
          {#if buyerError}
            <p class="qjd__err" data-testid="pricing-job-buyer-error">
              {buyerError}
            </p>
          {/if}

          <div class="qjd__leadtime-edit" data-testid="pricing-job-margin-edit">
            <input
              type="number"
              min="0"
              step="any"
              class="qjd__matselect"
              bind:value={marginDraft}
              disabled={marginBusy}
              placeholder="árrés % / margin %"
              aria-label="Margin override percent"
              data-testid="pricing-job-margin-input"
            />
            <button
              type="button"
              class="btn btn--secondary"
              onclick={saveMarginOverride}
              disabled={marginBusy}
              data-testid="pricing-job-margin-save"
              >{marginBusy ? "Mentés… / Saving…" : "Mentés / Save"}</button
            >
            <button
              type="button"
              class="btn btn--secondary"
              onclick={clearMarginOverride}
              disabled={marginBusy || detail.margin_override_pct === null}
              data-testid="pricing-job-margin-clear"
              >Felülírás törlése / Clear override</button
            >
          </div>
          {#if marginError}
            <p class="qjd__err" data-testid="pricing-job-margin-error">
              {marginError}
            </p>
          {/if}

          {#if floorConfirm !== null}
            <div
              class="qjd__floor-confirm"
              role="dialog"
              aria-label="Below min margin — proceed?"
              data-testid="pricing-job-margin-floor-confirm"
            >
              <p class="qjd__floor-confirm-text">
                <strong>Minimum árrés alatt — folytatja? / Below min margin —
                  proceed?</strong><br />
                Tényleges / Realized: {formatPercent(floorConfirm.realizedPct)},
                minimum / floor: {formatPercent(floorConfirm.floorPct)}. A DEAL
                ettől még tiltott marad. / DEAL stays blocked regardless.
              </p>
              <label class="qjd__field-label" for="floor-reason">
                Indok / Reason
              </label>
              <input
                id="floor-reason"
                type="text"
                class="qjd__matselect"
                bind:value={floorReason}
                disabled={marginBusy}
                placeholder="miért / why"
                data-testid="pricing-job-margin-floor-reason"
              />
              <div class="qjd__floor-confirm-actions">
                <button
                  type="button"
                  class="btn btn--secondary"
                  onclick={cancelFloorOverride}
                  disabled={marginBusy}>Mégse / Cancel</button
                >
                <button
                  type="button"
                  class="btn btn--danger"
                  onclick={confirmFloorOverride}
                  disabled={marginBusy || floorReason.trim().length === 0}
                  data-testid="pricing-job-margin-floor-proceed"
                  >Folytatás / Proceed</button
                >
              </div>
            </div>
          {/if}
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

      <!-- Footer: Retry (Failed rows). S416 — the operator
           accept-on-behalf affordance was removed; the customer accepts
           via the Accept button in their quote e-mail. -->
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
  /* S2 / ADR-0094 Gap 1 — stock-form dimensional inputs. Canonical
     warm-charcoal tokens (tokens.css); mono tabular figures for dims. */
  .qjd__stockdims {
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
    width: 100%;
  }
  .qjd__stockfield {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }
  .qjd__stockinput {
    background: var(--color-surface-sunken);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    border-radius: 6px;
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    width: 9ch;
  }
  /* S416 — the inline accept-on-behalf form CSS (.qjd__accept*) was
     removed along with the operator Accept affordance. */
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
  /* S427 — amber lead-time chip for the 8..21 day band. */
  .chip--warning {
    background: #78350f;
    color: #fed7aa;
  }
  /* S427 — lead-time override control row. */
  .qjd__leadtime-edit {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
    margin-top: 8px;
  }
  .qjd__leadtime-edit input {
    width: 120px;
  }

  /* S428 — margin-floor banner + buyer/override controls. */
  .qjd__floor-banner {
    margin: 8px 0;
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    font-size: var(--type-size-sm);
    line-height: 1.4;
  }
  .qjd__floor-banner strong {
    color: var(--color-signal-negative);
  }
  .qjd__buyer-edit {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-top: 8px;
  }
  .qjd__field-label {
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .qjd__floor-confirm {
    margin-top: 8px;
    padding: var(--space-3);
    border: 1px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .qjd__floor-confirm-text {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
    line-height: 1.4;
  }
  .qjd__floor-confirm-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
  }
</style>
