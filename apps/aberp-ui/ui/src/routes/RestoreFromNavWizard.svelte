<script lang="ts">
  // S180 / PR-180 — NAV-as-DR restore wizard.
  //
  // The operator surface for "the local DuckDB is gone — pull our
  // year-of-record from NAV." Lives under the maintenance area at
  // `#/restore-from-nav` so it sits one click behind the topbar gear
  // alongside Tenant Settings + NAV Credentials (the rare-touch,
  // load-bearing-when-touched bucket).
  //
  // Two operator gates BEFORE the request fires:
  //   1. Year is integer in [2018, currentYear].
  //   2. The operator types the literal token `RESTORE` in the
  //      confirmation input. NOT localized — the brief calls this
  //      out as "operator-discipline ceremony"; a translated token
  //      would weaken the signal.
  //
  // Result panel renders the {restored, skipped, errored} counts +
  // the year + the elapsed time. The pre-flight copy is explicit
  // about what does NOT carry over (email-send status, paid status,
  // notes, storno reason — see [[dev-db-disposable]]) so the operator
  // is not surprised post-run.

  // S261 / PR-250 — the wizard is now a linear dry-run-first flow:
  //   year → Preview (read-only, computes would-import N/M/K + gap
  //   warnings + checksum) → Confirm (type RESTORE) → Done. Nothing
  //   writes until the operator confirms (acceptance #4 dry-run). A
  //   held restore lock (DB row, survives a crash) shows a blocking
  //   "Restore in progress" banner + disables Preview/Confirm per
  //   [[trust-code-not-operator]].
  import { onMount } from "svelte";

  import {
    listRestoredInvoices,
    restoreFromNavOutgoing,
    restoreFromNavPreview,
    restoreLockAbandon,
    restoreLockStatus,
    type RestoreLock,
    type RestorePreview,
    type RestoreSummary,
    type RestoredInvoice,
  } from "../lib/api";
  import {
    formatPreviewHeadline,
    formatRestoreSummary,
    hasGapWarnings,
    isPreviewNoOp,
    isRestoreConfirmed,
    MIN_RESTORE_YEAR,
    RESTORE_CONFIRMATION_TOKEN,
    validateYearInput,
    type WizardStep,
  } from "../lib/restore-wizard";

  // currentYear from the browser clock. The backend re-validates
  // against its own UTC clock so a divergent SPA clock cannot
  // smuggle a future year past the API.
  const currentYear = new Date().getUTCFullYear();

  let step: WizardStep = $state("year");
  let yearRaw: string = $state(String(currentYear));
  let confirmRaw: string = $state("");
  let busy: boolean = $state(false);
  let errorMessage: string | null = $state(null);
  let preview: RestorePreview | null = $state(null);
  let summary: RestoreSummary | null = $state(null);

  // Restore-lock banner — a held lock blocks Preview + Confirm.
  let lock: RestoreLock | null = $state(null);
  let abandonToken: string = $state("");

  // Already-restored panel — fire-once load to show the operator
  // what's already in the local mirror table.
  type RestoredLoad =
    | { kind: "loading" }
    | { kind: "ready"; rows: RestoredInvoice[] }
    | { kind: "error"; message: string };
  let restored: RestoredLoad = $state({ kind: "loading" });

  onMount(() => {
    void loadRestored();
    void refreshLock();
  });

  async function loadRestored() {
    restored = { kind: "loading" };
    try {
      const rows = await listRestoredInvoices();
      restored = { kind: "ready", rows };
    } catch (e: unknown) {
      restored = {
        kind: "error",
        message: e instanceof Error ? e.message : String(e),
      };
    }
  }

  async function refreshLock() {
    try {
      lock = await restoreLockStatus();
    } catch {
      // A status read failure must not wedge the wizard — the backend
      // route gates are the hard guarantee; leave the banner as-is.
    }
  }

  // Derived gate states.
  const yearStatus = $derived(validateYearInput(yearRaw, currentYear));
  const locked = $derived(lock !== null);
  const previewEnabled = $derived(
    !busy && !locked && yearStatus.kind === "ok",
  );
  const confirmEnabled = $derived(
    !busy && !locked && isRestoreConfirmed(confirmRaw),
  );
  const abandonEnabled = $derived(!busy && isRestoreConfirmed(abandonToken));

  function resetToYear() {
    step = "year";
    preview = null;
    summary = null;
    confirmRaw = "";
    errorMessage = null;
  }

  async function onPreview(event: SubmitEvent) {
    event.preventDefault();
    if (!previewEnabled) return;
    const parsed = validateYearInput(yearRaw, currentYear);
    if (parsed.kind !== "ok") return;
    busy = true;
    errorMessage = null;
    preview = null;
    summary = null;
    try {
      // Read-only dry run — writes NOTHING. Computes would-import
      // counts + gap warnings + checksum against live NAV + local DB.
      preview = await restoreFromNavPreview(parsed.year);
      step = "preview";
    } catch (e: unknown) {
      errorMessage = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function onConfirm(event: SubmitEvent) {
    event.preventDefault();
    if (!confirmEnabled) return;
    const parsed = validateYearInput(yearRaw, currentYear);
    if (parsed.kind !== "ok") return;
    busy = true;
    errorMessage = null;
    try {
      // The token reached the backend verbatim so the server-side
      // gate (mirror of `isRestoreConfirmed`) passes; this is the
      // first write of the whole flow.
      summary = await restoreFromNavOutgoing(parsed.year, confirmRaw);
      step = "done";
      confirmRaw = "";
      await loadRestored();
      await refreshLock();
    } catch (e: unknown) {
      errorMessage = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function onAbandon(event: SubmitEvent) {
    event.preventDefault();
    if (!abandonEnabled) return;
    busy = true;
    errorMessage = null;
    try {
      await restoreLockAbandon(abandonToken);
      abandonToken = "";
      await refreshLock();
    } catch (e: unknown) {
      errorMessage = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<section class="wizard" aria-labelledby="restore-wizard-title">
  <header class="wizard__head">
    <h2 id="restore-wizard-title" class="wizard__title">
      Restore from NAV
    </h2>
    <p class="wizard__lede">
      Disaster recovery only. Use this if your local database was lost
      and you need to pull the regulated invoice records back from
      NAV's Online Számla view. NAV-stored invoices will be re-inserted
      with new local IDs.
    </p>
  </header>

  <section class="wizard__warning" role="note">
    <h3>What does NOT carry over</h3>
    <ul>
      <li>Email-send status (re-sending is a separate operator action)</li>
      <li>Paid / outstanding status</li>
      <li>Per-line and per-invoice notes</li>
      <li>Storno reasons</li>
      <li>Customer details (NAV's digest does not carry buyer info on this v1 path)</li>
      <li>Modify / storno chains between restored invoices</li>
    </ul>
    <p class="wizard__note">
      Each restored row is a recovered VIEW of what NAV holds, stored
      in the <code>restored_invoice</code> table — not the canonical
      <code>invoice</code> table. The two surfaces stay distinct so
      the regulated audit chain for invoices you ISSUED on this tenant
      is not muddied by recovered views.
    </p>
  </section>

  {#if locked && lock !== null}
    <section class="wizard__lock" role="alert" aria-live="assertive">
      <h3>⚠ Restore in progress</h3>
      <p>
        A restore from NAV is in progress (started {lock.acquired_at} by
        {lock.operator} for year {lock.year}). Issuing invoices and AP
        sync are blocked until it completes. If a previous run crashed
        mid-restore, abandon the lock below — a fresh run is idempotent
        and will simply re-pull what is missing.
      </p>
      <form class="wizard__abandon" onsubmit={onAbandon}>
        <label class="wizard__field">
          <span class="wizard__label">
            Type <code>{RESTORE_CONFIRMATION_TOKEN}</code> to abandon the
            held lock
          </span>
          <input
            type="text"
            bind:value={abandonToken}
            disabled={busy}
            autocomplete="off"
            spellcheck={false}
          />
        </label>
        <button type="submit" disabled={!abandonEnabled}>
          {#if busy}Working…{:else}Abandon restore lock{/if}
        </button>
      </form>
    </section>
  {/if}

  {#if errorMessage !== null}
    <section class="wizard__error" role="alert">
      <strong>Something went wrong.</strong>
      <pre>{errorMessage}</pre>
    </section>
  {/if}

  {#if step === "year"}
    <form class="wizard__form" onsubmit={onPreview}>
      <label class="wizard__field">
        <span class="wizard__label">Year to restore</span>
        <input
          type="text"
          inputmode="numeric"
          bind:value={yearRaw}
          disabled={busy || locked}
          aria-invalid={yearStatus.kind !== "ok"}
          aria-describedby={yearStatus.kind !== "ok" ? "year-error" : undefined}
        />
        {#if yearStatus.kind === "below_floor"}
          <p id="year-error" class="wizard__field-error">
            Year must be {MIN_RESTORE_YEAR} or later — NAV Online Számla
            went live in 2018; pre-2018 invoices were never submitted.
          </p>
        {:else if yearStatus.kind === "above_ceiling"}
          <p id="year-error" class="wizard__field-error">
            Year must be {yearStatus.ceiling} or earlier — NAV cannot
            hold invoices issued in the future.
          </p>
        {:else if yearStatus.kind === "not_integer"}
          <p id="year-error" class="wizard__field-error">
            Enter a 4-digit year (e.g. {currentYear}).
          </p>
        {/if}
      </label>
      <button type="submit" disabled={!previewEnabled}>
        {#if busy}Checking NAV…{:else}Preview restore{/if}
      </button>
      <p class="wizard__field-help">
        Preview is a read-only dry run — it walks NAV and tells you what
        a restore WOULD import. Nothing is written until you confirm.
      </p>
    </form>
  {/if}

  {#if step === "preview" && preview !== null}
    <section class="wizard__preview" aria-live="polite">
      <h3>Preview — year {preview.year}</h3>
      <p class="wizard__preview-headline">{formatPreviewHeadline(preview)}</p>
      <dl class="wizard__stats">
        <div><dt>NAV invoices for year</dt><dd>{preview.nav_invoice_count}</dd></div>
        <div><dt>New (would import)</dt><dd>{preview.new_invoice_count}</dd></div>
        <div><dt>Already present (skip)</dt><dd>{preview.already_present_count}</dd></div>
        <div><dt>New partners</dt><dd>{preview.new_partner_count}</dd></div>
        <div><dt>New products</dt><dd>{preview.new_product_count}</dd></div>
        <div><dt>Checksum (NAV set)</dt><dd class="mono">{preview.checksum}</dd></div>
      </dl>

      {#if preview.extraction_errored > 0}
        <p class="wizard__field-error">
          {preview.extraction_errored} invoice(s) could not be sampled
          for partner/product counts (NAV queryInvoiceData failed) — the
          invoice + checksum counts are unaffected, but the partner /
          product preview may understate the real import.
        </p>
      {/if}

      {#if hasGapWarnings(preview)}
        <section class="wizard__gaps" role="note">
          <h4>⚠ Gap warning — NAV is missing invoice numbers</h4>
          <p class="wizard__note">
            NAV's returned set has missing serial numbers the sequence
            implies should exist. This may be legitimate (voided numbers,
            multi-tool numbering) or a sign NAV is itself missing an
            invoice. Review before continuing.
          </p>
          <ul class="wizard__gap-list">
            {#each preview.gaps as g (g.series_prefix + g.missing_number)}
              <li class="mono">{g.series_prefix}{g.missing_number}</li>
            {/each}
          </ul>
          {#if preview.gaps_truncated}
            <p class="wizard__field-error">
              Gap list truncated — more missing numbers exist than shown.
            </p>
          {/if}
        </section>
      {/if}

      <div class="wizard__actions">
        <button type="button" class="secondary" onclick={resetToYear}>
          Back
        </button>
        {#if isPreviewNoOp(preview)}
          <p class="wizard__note">
            Nothing to import — the local database already holds every
            NAV invoice for this year. Re-running is safe but would be a
            no-op.
          </p>
        {:else}
          <button
            type="button"
            disabled={busy || locked}
            onclick={() => (step = "confirm")}
          >
            Continue to confirm
          </button>
        {/if}
      </div>
    </section>
  {/if}

  {#if step === "confirm" && preview !== null}
    <form class="wizard__form" onsubmit={onConfirm}>
      <p class="wizard__preview-headline">{formatPreviewHeadline(preview)}</p>
      <label class="wizard__field">
        <span class="wizard__label">
          Type <code>{RESTORE_CONFIRMATION_TOKEN}</code> to confirm and
          write
        </span>
        <input
          type="text"
          bind:value={confirmRaw}
          disabled={busy || locked}
          autocomplete="off"
          spellcheck={false}
          aria-describedby="restore-help"
        />
        <p id="restore-help" class="wizard__field-help">
          Operator-discipline ceremony — the token must be typed exactly
          as shown (uppercase, no surrounding whitespace). This is the
          first step that writes to your database.
        </p>
      </label>
      <div class="wizard__actions">
        <button type="button" class="secondary" onclick={() => (step = "preview")}>
          Back
        </button>
        <button type="submit" disabled={!confirmEnabled}>
          {#if busy}Restoring from NAV…{:else}Run restore{/if}
        </button>
      </div>
    </form>
  {/if}

  {#if step === "done" && summary !== null}
    <section class="wizard__result" aria-live="polite">
      <!-- S264 / PR-253 (F3) — a run with errored digests is NOT a
           clean restore; the heading must say so rather than claim
           "complete" with the failures buried in the count line. -->
      <h3>{summary.errored > 0 ? "Restore completed with errors" : "Restore complete"}</h3>
      <p>{formatRestoreSummary(summary)}</p>
      {#if summary.checksum}
        <p class="wizard__note">
          NAV invoice-number checksum:
          <span class="mono">{summary.checksum}</span>
          — recorded on the audit ledger (RestoreFromNavRun). Keep it to
          prove the restored set matches NAV.
        </p>
      {/if}
      {#if summary.errored > 0}
        <p class="wizard__field-error">
          {summary.errored} digest(s) failed to process — check the
          audit log for the verbatim NAV diagnostic per row.
        </p>
      {/if}
      <button type="button" onclick={resetToYear}>Start another</button>
    </section>
  {/if}

  <section class="wizard__restored">
    <h3>Already restored</h3>
    {#if restored.kind === "loading"}
      <p>Loading…</p>
    {:else if restored.kind === "error"}
      <p class="wizard__field-error">Load failed: {restored.message}</p>
    {:else if restored.rows.length === 0}
      <p class="wizard__note">
        No invoices have been restored yet. A successful run will
        populate this list.
      </p>
    {:else}
      <p class="wizard__note">
        {restored.rows.length} restored row{restored.rows.length === 1 ? "" : "s"}.
        Listed newest issue-date first.
      </p>
      <table class="wizard__table">
        <thead>
          <tr>
            <th scope="col">Invoice number</th>
            <th scope="col">Issued</th>
            <th scope="col" class="num">Gross</th>
            <th scope="col">Currency</th>
            <th scope="col">Year</th>
          </tr>
        </thead>
        <tbody>
          {#each restored.rows as r (r.id)}
            <tr>
              <td>{r.source_nav_invoice_number}</td>
              <td>{r.issue_date}</td>
              <td class="num">{r.total_gross_minor}</td>
              <td>{r.currency}</td>
              <td>{r.restore_year}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </section>
</section>

<style>
  .wizard {
    max-width: 880px;
    margin: 0 auto;
  }
  .wizard__head {
    margin-bottom: var(--space-4);
  }
  .wizard__title {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-lg);
    font-weight: 600;
    color: var(--color-text-strong);
  }
  .wizard__lede {
    margin: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
    max-width: 60ch;
  }
  .wizard__warning {
    margin: var(--space-4) 0;
    padding: var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-md);
  }
  .wizard__warning h3 {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-muted);
  }
  .wizard__warning ul {
    margin: 0 0 var(--space-2) var(--space-4);
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }
  .wizard__warning li {
    margin: var(--space-1) 0;
  }
  .wizard__note {
    margin: var(--space-2) 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
  }
  .wizard__form {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin: var(--space-4) 0;
  }
  .wizard__field {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .wizard__label {
    font-size: var(--type-size-sm);
    color: var(--color-text-strong);
  }
  .wizard__field input {
    padding: var(--space-2);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-md);
  }
  .wizard__field input[aria-invalid="true"] {
    border-color: var(--color-signal-negative);
  }
  .wizard__field-error {
    margin: 0;
    color: var(--color-signal-negative);
    font-size: var(--type-size-xs);
  }
  .wizard__field-help {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }
  .wizard__form button {
    align-self: flex-start;
    padding: var(--space-2) var(--space-4);
    background: var(--color-text-strong);
    color: var(--color-surface-raised);
    border: none;
    border-radius: var(--radius-sm);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    cursor: pointer;
  }
  .wizard__form button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .wizard__error {
    margin: var(--space-3) 0;
    padding: var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-signal-negative);
    border-radius: var(--radius-sm);
    color: var(--color-signal-negative);
  }
  .wizard__error pre {
    margin: var(--space-2) 0 0 0;
    white-space: pre-wrap;
    font-size: var(--type-size-xs);
  }
  .wizard__result {
    margin: var(--space-3) 0;
    padding: var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-signal-positive);
    border-radius: var(--radius-sm);
  }
  .wizard__result h3 {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }
  .wizard__restored {
    margin-top: var(--space-5);
  }
  .wizard__restored h3 {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-muted);
  }
  .wizard__table {
    width: 100%;
    border-collapse: collapse;
    margin-top: var(--space-2);
    font-size: var(--type-size-sm);
  }
  .wizard__table th,
  .wizard__table td {
    padding: var(--space-2);
    border-bottom: 1px solid var(--color-surface-divider);
    text-align: left;
  }
  .wizard__table th {
    color: var(--color-text-muted);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .wizard__table .num {
    text-align: right;
    font-variant-numeric: tabular-nums;
    font-family: var(--type-family-mono);
  }

  /* S261 / PR-250 — lock banner, preview panel, gap warnings. */
  .wizard__lock {
    margin: var(--space-3) 0;
    padding: var(--space-3);
    background: var(--color-surface-raised);
    border: 2px solid var(--color-signal-negative);
    border-radius: var(--radius-md);
  }
  .wizard__lock h3 {
    margin: 0 0 var(--space-2) 0;
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }
  .wizard__lock p {
    margin: 0 0 var(--space-2) 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
  }
  .wizard__abandon {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .wizard__abandon button,
  .wizard__actions button,
  .wizard__result button {
    align-self: flex-start;
    padding: var(--space-2) var(--space-4);
    background: var(--color-text-strong);
    color: var(--color-surface-raised);
    border: none;
    border-radius: var(--radius-sm);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    cursor: pointer;
  }
  .wizard__abandon button:disabled,
  .wizard__actions button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .wizard__actions {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin-top: var(--space-3);
    flex-wrap: wrap;
  }
  .wizard__actions button.secondary,
  .wizard__form button.secondary {
    background: transparent;
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
  }
  .wizard__preview,
  .wizard__result {
    margin: var(--space-3) 0;
    padding: var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
  }
  .wizard__preview h3 {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-muted);
  }
  .wizard__preview-headline {
    margin: 0 0 var(--space-3) 0;
    color: var(--color-text-strong);
    font-size: var(--type-size-md);
    line-height: 1.5;
  }
  .wizard__stats {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: var(--space-2);
    margin: 0 0 var(--space-3) 0;
  }
  .wizard__stats div {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .wizard__stats dt {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .wizard__stats dd {
    margin: 0;
    color: var(--color-text-strong);
    font-size: var(--type-size-md);
    font-variant-numeric: tabular-nums;
  }
  .mono {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    word-break: break-all;
  }
  .wizard__gaps {
    margin: var(--space-3) 0;
    padding: var(--space-3);
    border: 1px solid var(--color-signal-negative);
    border-radius: var(--radius-sm);
  }
  .wizard__gaps h4 {
    margin: 0 0 var(--space-2) 0;
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm);
  }
  .wizard__gap-list {
    margin: var(--space-2) 0 0 var(--space-4);
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    max-height: 200px;
    overflow-y: auto;
  }
</style>
