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
    deleteQuotePricingJob,
    fetchQuotePipelineStatus,
    listQuotePricingJobs,
    retryQuotePricingJob,
    type PipelinePythonStatus,
    type PricingJobRow,
  } from "../lib/api";
  import { formatInvoiceDate } from "../lib/format";
  import { classifyEmptyState } from "../lib/pricing-empty-state";
  import { customerCell } from "../lib/pricing-customer-cell";
  import { failureKindBadge } from "../lib/pricing-failure-kind";
  // S411 — sort + filter helpers (ports the PR-94 invoice-list pattern).
  // All sorting + filtering is client-side over the already-fetched list
  // ([[no-sql-specific]]); the helpers are pure + vitest-pinned in
  // `pricing-jobs-list.test.ts`.
  import {
    EMPTY_PRICING_FILTER,
    filterJobs,
    isPricingFilterEmpty,
    sortJobs,
    type PricingFilterSpec,
    type PricingSortKey,
    type PricingStateFacet,
    type SortDir,
  } from "../lib/pricing-jobs-list";
  import PricingJobDetail from "./PricingJobDetail.svelte";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState: LoadState = $state("idle");
  let errorMessage = $state<string | null>(null);
  let rows: PricingJobRow[] = $state([]);
  let retryBusyQuoteId = $state<string | null>(null);
  let retryError = $state<string | null>(null);
  // S391/F — Delete a permanently-Failed row. `confirmDeleteQuoteId` holds
  // the row awaiting the confirmation modal (null = modal closed);
  // `deleteBusyQuoteId` disables the modal's buttons mid-request.
  let confirmDeleteQuoteId = $state<string | null>(null);
  let deleteBusyQuoteId = $state<string | null>(null);
  let deleteError = $state<string | null>(null);
  // S349 / PR-40 (U1) — selected row drives the detail panel. `null`
  // keeps it closed; a row click sets the quote_id. In-memory Svelte
  // state (never browser storage); the panel closes on Esc / backdrop /
  // X / tab-change (route unmount) per its native-<dialog> semantics.
  let selectedQuoteId = $state<string | null>(null);
  // S282 / PR-267 — pipeline daemon status. Fetched in parallel with the
  // jobs list so the empty-state copy can differentiate dormant /
  // active / errored. Null until first fetch returns.
  let pipelineStatus = $state<PipelinePythonStatus | null>(null);

  // S411 — sort + filter state. TRANSIENT (not persisted): a pricing
  // queue is short-lived work-in-flight, not a long-lived ledger view
  // the operator returns to (unlike the invoice list's PR-175 prefs), so
  // each open starts at the operator-default view. Default sort = newest
  // first (the freshest failures + just-priced rows surface at the top).
  let filter: PricingFilterSpec = $state({ ...EMPTY_PRICING_FILTER });
  let sort: { key: PricingSortKey; dir: SortDir } = $state({
    key: "updated_at",
    dir: "desc",
  });

  // S411 — filter → sort composition. Mirrors `InvoiceList.svelte`'s
  // `visibleRows` derivation: facet + needle AND-filter, then a stable
  // per-column sort. Both helpers return new arrays so `rows` stays the
  // source of truth.
  let visibleRows = $derived(sortJobs(filterJobs(rows, filter), sort.key, sort.dir));

  // S411 — closed-vocab state chips. Only the three REACHABLE pricing
  // states get a chip (see `pricing-jobs-list.ts` docblock for why
  // Refused / Archived from the brief are dead controls on this tab).
  // "Mind / All" resets the facet; "Sikertelen / Failed" is the
  // attention bucket ([[hulye-biztos]] — one click to "what needs me").
  const STATE_CHIPS: { facet: PricingStateFacet; label: string }[] = [
    { facet: "All", label: "Mind / All" },
    { facet: "pending", label: "Folyamatban / Pending" },
    { facet: "posted", label: "Elküldve / Posted" },
    { facet: "failed", label: "Sikertelen / Failed" },
  ];

  // S411 — three-click sort cycle on a column header: a different column
  // jumps to (clicked, asc); same column toggles asc → desc → back to
  // the (updated_at, desc) default. Glyph ▲ / ▼ is the load-bearing
  // categorical signal per ADR-0017 (not colour alone).
  function onSortClick(key: PricingSortKey): void {
    if (sort.key !== key) {
      sort = { key, dir: "asc" };
      return;
    }
    if (sort.dir === "asc") {
      sort = { key, dir: "desc" };
      return;
    }
    sort = { key: "updated_at", dir: "desc" };
  }

  function sortIndicator(key: PricingSortKey): string {
    if (sort.key !== key) return "";
    return sort.dir === "asc" ? "▲" : "▼";
  }

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

  // S391/F — open the confirmation modal for a Failed row.
  function askDelete(quoteId: string): void {
    deleteError = null;
    confirmDeleteQuoteId = quoteId;
  }

  function cancelDelete(): void {
    if (deleteBusyQuoteId) return;
    confirmDeleteQuoteId = null;
  }

  async function onConfirmDelete(): Promise<void> {
    const quoteId = confirmDeleteQuoteId;
    if (!quoteId) return;
    deleteBusyQuoteId = quoteId;
    deleteError = null;
    try {
      await deleteQuotePricingJob(quoteId);
      confirmDeleteQuoteId = null;
      await refresh();
    } catch (e) {
      deleteError = e instanceof Error ? e.message : String(e);
    } finally {
      deleteBusyQuoteId = null;
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

  <!-- S411 — sort + filter controls. Only rendered once the table has
       rows to act on; the daemon-dormant / empty-active cards below own
       the no-rows surfaces. Search matches Ref / customer name / company
       / material; the chips facet by reachable pipeline state. -->
  {#if loadState === "ready" && rows.length > 0}
    <div class="pricing-jobs__controls">
      <label class="pricing-jobs__search">
        <span class="pricing-jobs__search-lbl">Keresés / Search</span>
        <input
          type="search"
          value={filter.search}
          oninput={(e) =>
            (filter = {
              ...filter,
              search: (e.currentTarget as HTMLInputElement).value,
            })}
          placeholder="Ref, vevő, cég, anyag… / Ref, customer, company, material…"
          autocomplete="off"
          spellcheck="false"
          aria-label="Keresés ajánlatok között / Search pricing jobs"
          data-testid="pricing-jobs-search"
        />
      </label>
      <div class="pricing-jobs__chips" role="group" aria-label="Állapot szűrő / State filter">
        {#each STATE_CHIPS as chip (chip.facet)}
          <button
            type="button"
            class="pricing-jobs__chip-btn"
            class:pricing-jobs__chip-btn--active={filter.state === chip.facet}
            aria-pressed={filter.state === chip.facet}
            onclick={() => (filter = { ...filter, state: chip.facet })}
            data-testid={`pricing-jobs-chip-${chip.facet}`}
          >{chip.label}</button>
        {/each}
      </div>
    </div>
  {/if}

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
          <!-- S411 — sortable headers (Customer / State / Price /
               Updated). The button carries the click + the ▲/▼ glyph;
               `aria-sort` announces the active column to screen readers.
               Material / Qty / Error stay plain (no natural ordering the
               brief named). -->
          <th
            aria-sort={sort.key === "customer"
              ? sort.dir === "asc"
                ? "ascending"
                : "descending"
              : "none"}
          >
            <button
              type="button"
              class="pricing-jobs__sort"
              onclick={() => onSortClick("customer")}
              data-testid="pricing-jobs-sort-customer"
            >
              <span>Vevő / Customer</span>
              <span class="pricing-jobs__sort-ind" aria-hidden="true">{sortIndicator("customer")}</span>
            </button>
          </th>
          <th>Anyag / Material</th>
          <th>Db / Qty</th>
          <th
            aria-sort={sort.key === "state"
              ? sort.dir === "asc"
                ? "ascending"
                : "descending"
              : "none"}
          >
            <button
              type="button"
              class="pricing-jobs__sort"
              onclick={() => onSortClick("state")}
              data-testid="pricing-jobs-sort-state"
            >
              <span>Állapot / State</span>
              <span class="pricing-jobs__sort-ind" aria-hidden="true">{sortIndicator("state")}</span>
            </button>
          </th>
          <th
            aria-sort={sort.key === "price"
              ? sort.dir === "asc"
                ? "ascending"
                : "descending"
              : "none"}
          >
            <button
              type="button"
              class="pricing-jobs__sort"
              onclick={() => onSortClick("price")}
              data-testid="pricing-jobs-sort-price"
            >
              <span>Ár / Price (EUR)</span>
              <span class="pricing-jobs__sort-ind" aria-hidden="true">{sortIndicator("price")}</span>
            </button>
          </th>
          <th>Hiba / Error</th>
          <th
            aria-sort={sort.key === "updated_at"
              ? sort.dir === "asc"
                ? "ascending"
                : "descending"
              : "none"}
          >
            <button
              type="button"
              class="pricing-jobs__sort"
              onclick={() => onSortClick("updated_at")}
              data-testid="pricing-jobs-sort-updated"
            >
              <span>Frissítve / Updated</span>
              <span class="pricing-jobs__sort-ind" aria-hidden="true">{sortIndicator("updated_at")}</span>
            </button>
          </th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        <!-- S411 — facet-aware empty state: rows exist but none match the
             active filter. The "Clear filters" button resets the search +
             facet in one click; surfaced only when a filter is engaged
             (CLAUDE.md rule 12 — no no-op affordance). colspan spans all
             9 columns. -->
        {#if visibleRows.length === 0}
          <tr class="pricing-jobs__empty-row" data-testid="pricing-jobs-empty-filtered">
            <td colspan="9">
              Nincs a szűrőnek megfelelő sor. / No rows match the current filter.
              {#if !isPricingFilterEmpty(filter)}
                <button
                  type="button"
                  class="btn btn--secondary pricing-jobs__clear"
                  onclick={() => (filter = { ...EMPTY_PRICING_FILTER })}
                  data-testid="pricing-jobs-clear-filter"
                >Szűrők törlése / Clear filters</button>
              {/if}
            </td>
          </tr>
        {/if}
        {#each visibleRows as row (row.quote_id)}
          <!-- S401 — resolve the Customer-cell shape once per row;
               {@const} must be a direct child of the {#each} block. -->
          {@const cust = customerCell(
            row.customer_company,
            row.customer_name,
            row.customer_email,
          )}
          <tr
            data-testid={`pricing-jobs-row-${row.quote_id}`}
            class="pricing-jobs__row"
            tabindex="0"
            role="button"
            aria-label={`Részletek / Details ${row.quote_id}`}
            onclick={() => (selectedQuoteId = row.quote_id)}
            onkeydown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                selectedQuoteId = row.quote_id;
              }
            }}
          >
            <td>
              <code title={row.quote_id}>{shortQuoteId(row.quote_id)}</code>
              {#if row.attempt_n > 0}
                <span class="pricing-jobs__attempt">×{row.attempt_n}</span>
              {/if}
            </td>
            <td>
              <!-- S401 — company is the operator's primary anchor (who
                   they're quoting); person + email sit below it muted. -->
              <div
                class="pricing-jobs__company"
                class:pricing-jobs__company--missing={cust.companyMissing}
                data-testid={`pricing-jobs-company-${row.quote_id}`}
              >{cust.company}</div>
              <div class="pricing-jobs__muted">{cust.person}</div>
              <div class="pricing-jobs__muted">{cust.email}</div>
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
                  onclick={(e) => {
                    e.stopPropagation();
                    void onRetry(row.quote_id);
                  }}
                  disabled={retryBusyQuoteId === row.quote_id}
                  data-testid={`pricing-jobs-retry-${row.quote_id}`}
                >
                  {retryBusyQuoteId === row.quote_id
                    ? "Újrapróbálás… / Retrying…"
                    : "Újra / Retry"}
                </button>
                <!-- S391/F — Delete is only offered on Failed rows; opens a
                     confirmation modal before the DELETE fires. -->
                <button
                  type="button"
                  class="btn btn--danger"
                  onclick={(e) => {
                    e.stopPropagation();
                    askDelete(row.quote_id);
                  }}
                  disabled={deleteBusyQuoteId === row.quote_id}
                  data-testid={`pricing-jobs-delete-${row.quote_id}`}
                >Törlés / Delete</button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  <!-- S391/F — delete confirmation modal. Dark-themed overlay; the
       operator must confirm before the irreversible DELETE fires. Esc /
       backdrop cancel. -->
  {#if confirmDeleteQuoteId}
    <div
      class="pricing-jobs__modal-backdrop"
      role="presentation"
      onclick={cancelDelete}
    >
      <div
        class="pricing-jobs__modal"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="pricing-jobs-delete-title"
        tabindex="-1"
        onclick={(e) => e.stopPropagation()}
        onkeydown={(e) => {
          if (e.key === "Escape") cancelDelete();
        }}
        data-testid="pricing-jobs-delete-modal"
      >
        <h3 id="pricing-jobs-delete-title">Sor törlése / Delete row</h3>
        <p>
          Biztosan törlöd ezt a sikertelen árazási sort? A művelet nem
          vonható vissza. / Delete this failed pricing row? This cannot be
          undone.
        </p>
        <p class="pricing-jobs__modal-id">
          <code>{confirmDeleteQuoteId}</code>
        </p>
        {#if deleteError}
          <p class="pricing-jobs__err" data-testid="pricing-jobs-delete-error">
            {deleteError}
          </p>
        {/if}
        <div class="pricing-jobs__modal-actions">
          <button
            type="button"
            class="btn btn--secondary"
            onclick={cancelDelete}
            disabled={deleteBusyQuoteId !== null}
            data-testid="pricing-jobs-delete-cancel"
          >Mégse / Cancel</button>
          <button
            type="button"
            class="btn btn--danger"
            onclick={() => void onConfirmDelete()}
            disabled={deleteBusyQuoteId !== null}
            data-testid="pricing-jobs-delete-confirm"
          >
            {deleteBusyQuoteId !== null
              ? "Törlés… / Deleting…"
              : "Törlés / Delete"}
          </button>
        </div>
      </div>
    </div>
  {/if}

  <!-- S349 / PR-40 (U1) — row-detail panel. Mounted once; the
       `quoteId` prop toggling between a string and `null` opens/closes
       the native <dialog>. -->
  <PricingJobDetail
    quoteId={selectedQuoteId}
    onClose={() => (selectedQuoteId = null)}
    onRetried={() => void refresh()}
  />
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
    border-radius: var(--radius-md);
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
    border-radius: var(--radius-md);
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
  /* S411 — sort + filter controls bar. */
  .pricing-jobs__controls {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 12px;
    margin-bottom: 12px;
  }
  .pricing-jobs__search {
    display: flex;
    flex-direction: column;
    gap: 2px;
    flex: 1 1 280px;
    min-width: 220px;
  }
  .pricing-jobs__search-lbl {
    font-size: 11px;
    color: var(--color-text-muted, #9ca3af);
  }
  .pricing-jobs__search input {
    background: var(--color-surface, #1f2937);
    color: var(--color-text, #e5e7eb);
    border: 1px solid var(--color-border, #374151);
    border-radius: var(--radius-md);
    padding: 6px 10px;
    font-size: 13px;
  }
  .pricing-jobs__search input:focus-visible {
    outline: 2px solid var(--color-accent, #60a5fa);
    outline-offset: -1px;
  }
  .pricing-jobs__chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .pricing-jobs__chip-btn {
    padding: 5px 12px;
    border-radius: var(--radius-pill);
    font-size: 12px;
    font-weight: 500;
    cursor: pointer;
    background: var(--color-surface, #1f2937);
    color: var(--color-text-muted, #9ca3af);
    border: 1px solid var(--color-border, #374151);
  }
  .pricing-jobs__chip-btn:hover:not(.pricing-jobs__chip-btn--active) {
    background: var(--color-surface-2, #243042);
    color: var(--color-text, #e5e7eb);
  }
  .pricing-jobs__chip-btn--active {
    background: var(--color-accent, #2563eb);
    border-color: var(--color-accent, #2563eb);
    color: #f8fafc;
  }
  /* S411 — sortable column-header button. Quiet chrome (no border /
     background) so the header reads as a label until hovered; the ▲/▼
     glyph is the active-sort signal. */
  .pricing-jobs__sort {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    cursor: pointer;
    font: inherit;
    font-weight: 600;
    color: var(--color-text-muted, #9ca3af);
  }
  .pricing-jobs__sort:hover {
    color: var(--color-text, #e5e7eb);
  }
  .pricing-jobs__sort:focus-visible {
    outline: 2px solid var(--color-accent, #60a5fa);
    outline-offset: 2px;
  }
  .pricing-jobs__sort-ind {
    font-size: 10px;
    color: var(--color-accent, #60a5fa);
    min-width: 8px;
  }
  .pricing-jobs__empty-row td {
    color: var(--color-text-muted, #9ca3af);
    font-style: italic;
  }
  .pricing-jobs__clear {
    margin-left: 10px;
    font-style: normal;
  }
  /* S349 / PR-40 (U1) — clickable rows open the detail panel. */
  .pricing-jobs__row {
    cursor: pointer;
  }
  .pricing-jobs__row:hover {
    background: var(--color-surface-2, #243042);
  }
  .pricing-jobs__row:focus-visible {
    outline: 2px solid var(--color-accent, #60a5fa);
    outline-offset: -2px;
  }
  .pricing-jobs__muted {
    color: var(--color-text-muted, #9ca3af);
    font-size: 12px;
  }
  /* S401 — buyer's company is the operator's primary anchor: larger,
     full-weight, full-contrast vs. the muted person/email lines below. */
  .pricing-jobs__company {
    font-size: 14px;
    font-weight: 600;
    color: var(--color-text, #e5e7eb);
  }
  /* Legacy / blank-company rows render the placeholder muted + italic so
     the cell reads as "no company captured", not a bold real name. */
  .pricing-jobs__company--missing {
    font-weight: 400;
    font-style: italic;
    color: var(--color-text-muted, #9ca3af);
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
    border-radius: var(--radius-pill);
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
  /* S391/F — Delete button + confirmation modal. Dark-theme tokens with
     the same hard-coded fallbacks the rest of this panel uses. */
  .btn--danger {
    margin-left: 6px;
    background: var(--color-danger, #7f1d1d);
    color: #fecaca;
    border-color: var(--color-danger, #b91c1c);
  }
  .btn--danger:hover:not(:disabled) {
    background: #991b1b;
  }
  .pricing-jobs__modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 50;
  }
  .pricing-jobs__modal {
    background: var(--color-surface, #1f2937);
    color: var(--color-text, #e5e7eb);
    border: 1px solid var(--color-border, #374151);
    border-radius: var(--radius-md);
    padding: 20px;
    max-width: 460px;
    width: calc(100% - 32px);
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.5);
  }
  .pricing-jobs__modal h3 {
    margin: 0 0 8px;
    font-size: 16px;
  }
  .pricing-jobs__modal p {
    margin: 8px 0;
    font-size: 13px;
  }
  .pricing-jobs__modal-id {
    color: var(--color-text-muted, #9ca3af);
    word-break: break-all;
  }
  .pricing-jobs__modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
</style>
