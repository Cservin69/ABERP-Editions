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

  import { onMount } from "svelte";

  import {
    listRestoredInvoices,
    restoreFromNavOutgoing,
    type RestoreSummary,
    type RestoredInvoice,
  } from "../lib/api";
  import {
    canSubmit,
    formatRestoreSummary,
    MIN_RESTORE_YEAR,
    RESTORE_CONFIRMATION_TOKEN,
    validateYearInput,
  } from "../lib/restore-wizard";

  // currentYear from the browser clock. The backend re-validates
  // against its own UTC clock so a divergent SPA clock cannot
  // smuggle a future year past the API.
  const currentYear = new Date().getUTCFullYear();

  let yearRaw: string = $state(String(currentYear));
  let confirmRaw: string = $state("");
  let busy: boolean = $state(false);
  let errorMessage: string | null = $state(null);
  let summary: RestoreSummary | null = $state(null);

  // Already-restored panel — fire-once load to show the operator
  // what's already in the local mirror table.
  type RestoredLoad =
    | { kind: "loading" }
    | { kind: "ready"; rows: RestoredInvoice[] }
    | { kind: "error"; message: string };
  let restored: RestoredLoad = $state({ kind: "loading" });

  onMount(() => {
    void loadRestored();
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

  // Derived: gate states.
  const yearStatus = $derived(validateYearInput(yearRaw, currentYear));
  const submitEnabled = $derived(
    !busy && canSubmit(yearRaw, confirmRaw, currentYear),
  );

  async function onSubmit(event: SubmitEvent) {
    event.preventDefault();
    if (!submitEnabled) return;
    // Both gates already passed; coerce year to number.
    const parsed = validateYearInput(yearRaw, currentYear);
    if (parsed.kind !== "ok") return;
    busy = true;
    errorMessage = null;
    summary = null;
    try {
      const result = await restoreFromNavOutgoing(parsed.year);
      summary = result;
      // Refresh the already-restored panel so the new rows appear
      // without a manual reload.
      await loadRestored();
      // Clear the confirmation token so a second restore requires
      // re-typing RESTORE — every restore is its own ceremony.
      confirmRaw = "";
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

  <form class="wizard__form" onsubmit={onSubmit}>
    <label class="wizard__field">
      <span class="wizard__label">Year to restore</span>
      <input
        type="text"
        inputmode="numeric"
        bind:value={yearRaw}
        disabled={busy}
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

    <label class="wizard__field">
      <span class="wizard__label">
        Type <code>{RESTORE_CONFIRMATION_TOKEN}</code> to confirm
      </span>
      <input
        type="text"
        bind:value={confirmRaw}
        disabled={busy}
        autocomplete="off"
        spellcheck={false}
        aria-describedby="restore-help"
      />
      <p id="restore-help" class="wizard__field-help">
        Operator-discipline ceremony — pasting will not work, the token
        must be typed exactly as shown (uppercase, no surrounding
        whitespace).
      </p>
    </label>

    <button type="submit" disabled={!submitEnabled}>
      {#if busy}Restoring from NAV…{:else}Run restore{/if}
    </button>
  </form>

  {#if errorMessage !== null}
    <section class="wizard__error" role="alert">
      <strong>Restore failed.</strong>
      <pre>{errorMessage}</pre>
    </section>
  {/if}

  {#if summary !== null}
    <section class="wizard__result" aria-live="polite">
      <h3>Last run</h3>
      <p>{formatRestoreSummary(summary)}</p>
      {#if summary.errored > 0}
        <p class="wizard__field-error">
          {summary.errored} digest(s) failed to process — check the
          audit log for the verbatim NAV diagnostic per row.
        </p>
      {/if}
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
    border-radius: 6px;
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
    border-radius: 4px;
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
    border-radius: 4px;
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
    border-radius: 4px;
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
    border-radius: 4px;
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
</style>
