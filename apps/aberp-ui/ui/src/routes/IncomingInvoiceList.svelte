<script lang="ts">
  // PR-179 / session-179 — AP module v1 SPA surface. Renders the
  // `ap_invoice` mirror (S177) and exposes the three closed-vocab
  // status transitions (mark-paid / mark-outstanding / mark-irrelevant)
  // + the operator-clicked "Sync now" shortcut to the S178 daemon's
  // manual code path.
  //
  // Shape conventions inherited from `InvoiceList.svelte`:
  //   - dense table per ADR-0017 §3 (right-aligned numerics, mono
  //     totals, tabular-nums on monospaced columns).
  //   - sortable column headers via three-cycle (asc → desc → reset).
  //   - status chip = colour + glyph + label (categorical signal
  //     never carried by colour alone, ADR-0017 §"Adversarial #4").
  //   - sort + filter persisted to localStorage with closed-vocab
  //     discards (S175 posture), separate key from AR-side.
  //
  // What this PR explicitly defers:
  //   - per-row detail page (a click on supplier name reveals the
  //     full address in a tooltip — not enough operator demand for a
  //     full detail modal yet).
  //   - NAV XML download. The on-disk path is stored in
  //     `nav_xml_path`; the SPA does not expose it.
  //   - approve workflow / payment execution / PO matching. The brief
  //     is explicit: v1 is mirror + 3-state mark, nothing more.

  import { onMount } from "svelte";
  import {
    listIncomingInvoices,
    markIncomingPaid,
    markIncomingOutstanding,
    markIncomingIrrelevant,
    syncIncomingInvoicesNow,
    type IncomingInvoice,
    type SyncIncomingNowResponse,
    type Currency,
  } from "../lib/api";
  import { formatTotal, formatInvoiceDate } from "../lib/format";
  import {
    actionsForStatus,
    metaForStatus,
  } from "../lib/incoming-invoice-status";
  import {
    isIrrelevantReasonValid,
    syncCompletedToast,
  } from "../lib/incoming-invoice-actions";
  import {
    DEFAULT_INCOMING_LIST_PREFS,
    loadIncomingListPrefs,
    saveIncomingListPrefs,
    type IncomingSortKey,
    type IncomingFilterSpec,
    type SortDir,
  } from "../lib/incoming-invoice-list-persistence";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState: LoadState = $state("idle");
  let errorMessage = $state<string | null>(null);
  let invoices: IncomingInvoice[] = $state([]);

  // PR-175 posture — load persisted prefs once on first paint; every
  // mutation writes back on the same beat. The Svelte 5 derived
  // pipeline (`visibleInvoices`) reads `sort` + `filter` so the table
  // re-renders without extra wiring.
  let initialPrefs = loadIncomingListPrefs();
  let sort: { key: IncomingSortKey | null; dir: SortDir } = $state(
    initialPrefs.sort,
  );
  let filter: IncomingFilterSpec = $state(initialPrefs.filter);

  // Sync-now spinner + toast.
  let syncInFlight = $state(false);
  let syncToast: { hu: string; en: string; tone: "ok" | "error" } | null =
    $state(null);

  // Per-row in-flight markers (keyed by `incoming.id`); used to disable
  // the action buttons while a click is awaiting backend confirmation
  // so a double-click doesn't fire two transitions.
  let mutatingIds: Record<string, boolean> = $state({});

  // Mark-irrelevant modal state.
  let irrelevantDialogEl: HTMLDialogElement | undefined = $state();
  let irrelevantTarget: IncomingInvoice | null = $state(null);
  let irrelevantReason = $state("");
  let irrelevantError: string | null = $state(null);
  let irrelevantSubmitting = $state(false);

  onMount(() => {
    void refresh();
  });

  $effect(() => {
    // Persist on every mutation. Cheap synchronous JSON write; the
    // helper swallows quota/private-browsing throws.
    saveIncomingListPrefs({ sort, filter });
  });

  async function refresh() {
    loadState = "loading";
    errorMessage = null;
    try {
      invoices = await listIncomingInvoices();
      loadState = "ready";
    } catch (err: unknown) {
      errorMessage = err instanceof Error ? err.message : String(err);
      loadState = "error";
    }
  }

  // ── Sorting ────────────────────────────────────────────────────────

  function onSortClick(key: IncomingSortKey) {
    if (sort.key !== key) {
      sort = { key, dir: "asc" };
      return;
    }
    if (sort.dir === "asc") {
      sort = { key, dir: "desc" };
      return;
    }
    // Third click resets — same three-cycle posture as InvoiceList.
    sort = { ...DEFAULT_INCOMING_LIST_PREFS.sort };
  }

  function sortIndicator(key: IncomingSortKey): string {
    if (sort.key !== key) return "";
    return sort.dir === "asc" ? "▲" : "▼";
  }

  function compareIncoming(a: IncomingInvoice, b: IncomingInvoice): number {
    if (sort.key === null) {
      // Stable lifecycle-natural fallback: newest first (issue_date
      // desc, then created_at desc as the tie-break).
      const cmp = b.issue_date.localeCompare(a.issue_date);
      if (cmp !== 0) return cmp;
      return b.created_at.localeCompare(a.created_at);
    }
    const dir = sort.dir === "asc" ? 1 : -1;
    switch (sort.key) {
      case "supplier_name":
        return a.supplier_name.localeCompare(b.supplier_name) * dir;
      case "supplier_tax_number":
        return a.supplier_tax_number.localeCompare(b.supplier_tax_number) * dir;
      case "nav_invoice_number":
        return a.nav_invoice_number.localeCompare(b.nav_invoice_number) * dir;
      case "issue_date":
        return a.issue_date.localeCompare(b.issue_date) * dir;
      case "total_gross":
        return (a.total_gross_minor - b.total_gross_minor) * dir;
      case "local_status":
        return a.local_status.localeCompare(b.local_status) * dir;
    }
  }

  // ── Filtering ──────────────────────────────────────────────────────

  function matchesFilter(inv: IncomingInvoice): boolean {
    if (filter.status !== "All" && inv.local_status !== filter.status) {
      return false;
    }
    if (filter.currency !== "All" && inv.currency !== filter.currency) {
      return false;
    }
    const needle = filter.needle.trim().toLowerCase();
    if (needle === "") return true;
    return (
      inv.supplier_name.toLowerCase().includes(needle) ||
      inv.supplier_tax_number.toLowerCase().includes(needle) ||
      inv.nav_invoice_number.toLowerCase().includes(needle)
    );
  }

  let visibleInvoices = $derived(
    [...invoices].filter(matchesFilter).sort(compareIncoming),
  );

  // ── Status actions ─────────────────────────────────────────────────

  async function onMarkPaid(inv: IncomingInvoice) {
    if (mutatingIds[inv.id]) return;
    mutatingIds = { ...mutatingIds, [inv.id]: true };
    try {
      const resp = await markIncomingPaid(inv.id);
      applyStatusChange(inv.id, resp.to_status, null);
    } catch (err: unknown) {
      errorMessage = err instanceof Error ? err.message : String(err);
    } finally {
      const { [inv.id]: _drop, ...rest } = mutatingIds;
      mutatingIds = rest;
    }
  }

  async function onMarkOutstanding(inv: IncomingInvoice) {
    if (mutatingIds[inv.id]) return;
    mutatingIds = { ...mutatingIds, [inv.id]: true };
    try {
      const resp = await markIncomingOutstanding(inv.id);
      applyStatusChange(inv.id, resp.to_status, null);
    } catch (err: unknown) {
      errorMessage = err instanceof Error ? err.message : String(err);
    } finally {
      const { [inv.id]: _drop, ...rest } = mutatingIds;
      mutatingIds = rest;
    }
  }

  function applyStatusChange(
    id: string,
    toStatus: string,
    reason: string | null,
  ) {
    invoices = invoices.map((row) =>
      row.id === id
        ? { ...row, local_status: toStatus, irrelevant_reason: reason }
        : row,
    );
  }

  function openIrrelevant(inv: IncomingInvoice) {
    irrelevantTarget = inv;
    irrelevantReason = "";
    irrelevantError = null;
    irrelevantSubmitting = false;
    irrelevantDialogEl?.showModal();
  }

  function closeIrrelevant() {
    irrelevantDialogEl?.close();
    irrelevantTarget = null;
    irrelevantReason = "";
    irrelevantError = null;
    irrelevantSubmitting = false;
  }

  async function submitIrrelevant(e: SubmitEvent) {
    e.preventDefault();
    if (!irrelevantTarget) return;
    if (!isIrrelevantReasonValid(irrelevantReason)) {
      irrelevantError =
        "A nem releváns megjelölés indoklása kötelező. / Marking as Irrelevant requires a reason.";
      return;
    }
    irrelevantSubmitting = true;
    irrelevantError = null;
    try {
      const id = irrelevantTarget.id;
      const reason = irrelevantReason.trim();
      const resp = await markIncomingIrrelevant(id, reason);
      applyStatusChange(id, resp.to_status, resp.reason);
      closeIrrelevant();
    } catch (err: unknown) {
      irrelevantError = err instanceof Error ? err.message : String(err);
    } finally {
      irrelevantSubmitting = false;
    }
  }

  // ── Sync now ───────────────────────────────────────────────────────

  async function onSyncNowClick() {
    if (syncInFlight) return;
    syncInFlight = true;
    syncToast = null;
    try {
      const summary: SyncIncomingNowResponse = await syncIncomingInvoicesNow();
      if (summary.status === "ok") {
        const copy = syncCompletedToast(summary.ingested_count);
        syncToast = { ...copy, tone: "ok" };
        await refresh();
      } else {
        syncToast = {
          hu: `Szinkronizálás sikertelen: ${summary.error ?? "ismeretlen hiba"}`,
          en: `Sync failed: ${summary.error ?? "unknown error"}`,
          tone: "error",
        };
      }
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      syncToast = {
        hu: `Szinkronizálás sikertelen: ${message}`,
        en: `Sync failed: ${message}`,
        tone: "error",
      };
    } finally {
      syncInFlight = false;
    }
  }

  function dismissToast() {
    syncToast = null;
  }

  function clearFilters() {
    filter = { needle: "", status: "All", currency: "All" };
  }

  function asCurrency(raw: string): Currency | null {
    return raw === "HUF" || raw === "EUR" ? (raw as Currency) : null;
  }
</script>

<section class="screen">
  <div class="screen-head">
    <h2>Bejövő számlák / Incoming invoices</h2>
    <div class="actions">
      <label class="search">
        <span class="visually-hidden">Bejövő számla keresése</span>
        <input
          value={filter.needle}
          oninput={(e) =>
            (filter = {
              ...filter,
              needle: (e.currentTarget as HTMLInputElement).value,
            })}
          type="search"
          placeholder="Keresés szállító / NAV szám szerint…"
          autocomplete="off"
          spellcheck="false"
          aria-label="Bejövő számlák keresése"
        />
      </label>
      <label class="filter">
        <span class="filter-label">Státusz</span>
        <select
          value={filter.status}
          onchange={(e) =>
            (filter = {
              ...filter,
              status: (e.currentTarget as HTMLSelectElement).value as
                | "All"
                | "Outstanding"
                | "Paid"
                | "Irrelevant",
            })}
          aria-label="Szűrés státusz szerint"
        >
          <option value="All">All</option>
          <option value="Outstanding">Outstanding</option>
          <option value="Paid">Paid</option>
          <option value="Irrelevant">Irrelevant</option>
        </select>
      </label>
      <label class="filter">
        <span class="filter-label">Pénznem</span>
        <select
          value={filter.currency}
          onchange={(e) =>
            (filter = {
              ...filter,
              currency: (e.currentTarget as HTMLSelectElement).value as
                | "All"
                | Currency,
            })}
          aria-label="Szűrés pénznem szerint"
        >
          <option value="All">All</option>
          <option value="HUF">HUF</option>
          <option value="EUR">EUR</option>
        </select>
      </label>
      <button
        type="button"
        class="quiet-button"
        onclick={() => void refresh()}
        disabled={loadState === "loading"}
      >
        Frissít / Refresh
      </button>
      <button
        type="button"
        class="quiet-button primary"
        onclick={() => void onSyncNowClick()}
        disabled={syncInFlight}
        title="Lekér a NAV-tól minden új befelé számlát (auto-sync 30 percenként fut)"
      >
        {#if syncInFlight}
          Szinkronizálás folyamatban…
        {:else}
          Szinkronizálj most / Sync now
        {/if}
      </button>
    </div>
  </div>

  {#if syncToast !== null}
    <div
      class="toast"
      data-tone={syncToast.tone}
      role={syncToast.tone === "error" ? "alert" : "status"}
    >
      <span class="toast-text">{syncToast.hu}</span>
      <span class="toast-text toast-en">{syncToast.en}</span>
      <button
        type="button"
        class="toast-dismiss"
        onclick={dismissToast}
        aria-label="Üzenet bezárása"
      >×</button>
    </div>
  {/if}

  {#if loadState === "error" && errorMessage !== null}
    <p class="error" role="alert">{errorMessage}</p>
  {/if}

  <table class="dense">
    <thead>
      <tr>
        <th
          scope="col"
          class="col-supplier"
          aria-sort={sort.key === "supplier_name"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header"
            onclick={() => onSortClick("supplier_name")}
          >
            <span>Szállító / Supplier</span>
            <span class="sort-indicator" aria-hidden="true"
              >{sortIndicator("supplier_name")}</span
            >
          </button>
        </th>
        <th
          scope="col"
          class="col-tax"
          aria-sort={sort.key === "supplier_tax_number"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header"
            onclick={() => onSortClick("supplier_tax_number")}
          >
            <span>Adószám / Tax #</span>
            <span class="sort-indicator" aria-hidden="true"
              >{sortIndicator("supplier_tax_number")}</span
            >
          </button>
        </th>
        <th
          scope="col"
          class="col-nav-number"
          aria-sort={sort.key === "nav_invoice_number"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header"
            onclick={() => onSortClick("nav_invoice_number")}
          >
            <span>NAV számlaszám</span>
            <span class="sort-indicator" aria-hidden="true"
              >{sortIndicator("nav_invoice_number")}</span
            >
          </button>
        </th>
        <th
          scope="col"
          class="col-num"
          aria-sort={sort.key === "issue_date"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header right"
            onclick={() => onSortClick("issue_date")}
          >
            <span>Kelt / Issued</span>
            <span class="sort-indicator" aria-hidden="true"
              >{sortIndicator("issue_date")}</span
            >
          </button>
        </th>
        <th
          scope="col"
          class="col-num"
          aria-sort={sort.key === "total_gross"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header right"
            onclick={() => onSortClick("total_gross")}
          >
            <span>Bruttó / Gross</span>
            <span class="sort-indicator" aria-hidden="true"
              >{sortIndicator("total_gross")}</span
            >
          </button>
        </th>
        <th
          scope="col"
          class="col-status"
          aria-sort={sort.key === "local_status"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header"
            onclick={() => onSortClick("local_status")}
          >
            <span>Státusz</span>
            <span class="sort-indicator" aria-hidden="true"
              >{sortIndicator("local_status")}</span
            >
          </button>
        </th>
        <th scope="col" class="col-actions">Műveletek / Actions</th>
      </tr>
    </thead>
    <tbody>
      {#if visibleInvoices.length === 0}
        <tr class="empty-row">
          <td colspan="7">
            {#if invoices.length === 0 && loadState === "ready"}
              <p class="empty-hu">
                Még nincs bejövő számla. A NAV-ról automatikusan szinkronizálunk
                30 percenként; vagy nyomd meg most a „Szinkronizálj most” gombot.
              </p>
              <p class="empty-en">
                No incoming invoices yet. They'll appear here as NAV records
                supplier-issued invoices to you. The sync runs automatically
                every 30 minutes, or click "Sync now" to pull immediately.
              </p>
            {:else if loadState === "loading"}
              <p class="empty-en">Betöltés… / Loading…</p>
            {:else}
              <p class="empty-en">
                Nincs találat a szűrőre. / No invoices match the current filter.
              </p>
              <button
                type="button"
                class="quiet-button clear-filters"
                onclick={clearFilters}
              >
                Szűrők törlése / Clear filters
              </button>
            {/if}
          </td>
        </tr>
      {:else}
        {#each visibleInvoices as inv (inv.id)}
          {@const meta = metaForStatus(inv.local_status)}
          {@const allowed = actionsForStatus(inv.local_status)}
          {@const isMutating = mutatingIds[inv.id] === true}
          {@const cur = asCurrency(inv.currency)}
          <tr>
            <td>
              <span class="supplier-name" title={inv.supplier_address ?? ""}>
                {inv.supplier_name}
              </span>
            </td>
            <td class="mono">{inv.supplier_tax_number}</td>
            <td class="mono">{inv.nav_invoice_number}</td>
            <td class="mono col-num">{formatInvoiceDate(inv.issue_date)}</td>
            <td class="mono col-num">
              {#if cur !== null}
                {formatTotal(inv.total_gross_minor, cur)}
              {:else}
                <span class="muted-currency" title={`Currency ${inv.currency}`}>
                  {inv.total_gross_minor} {inv.currency}
                </span>
              {/if}
            </td>
            <td>
              <span
                class="status-chip"
                class:status-chip--outstanding={meta.cssClass === "outstanding"}
                class:status-chip--paid={meta.cssClass === "paid"}
                class:status-chip--irrelevant={meta.cssClass === "irrelevant"}
                class:status-chip--unknown={meta.cssClass === "unknown"}
                title={inv.local_status === "Irrelevant" &&
                inv.irrelevant_reason !== null
                  ? `Indok: ${inv.irrelevant_reason}`
                  : meta.label_en}
              >
                <span class="status-chip__glyph" aria-hidden="true"
                  >{meta.glyph}</span
                >
                <span class="status-chip__label">{meta.label_hu}</span>
              </span>
            </td>
            <td class="col-actions">
              {#if allowed.includes("mark-paid")}
                <button
                  type="button"
                  class="row-action"
                  disabled={isMutating}
                  onclick={() => void onMarkPaid(inv)}
                  title="Kifizetve megjelölése (Mark as paid)"
                >
                  Kifizetve
                </button>
              {/if}
              {#if allowed.includes("mark-outstanding")}
                <button
                  type="button"
                  class="row-action"
                  disabled={isMutating}
                  onclick={() => void onMarkOutstanding(inv)}
                  title="Vissza Outstanding állapotba"
                >
                  Vissza Outstanding-re
                </button>
              {/if}
              {#if allowed.includes("mark-irrelevant")}
                <button
                  type="button"
                  class="row-action"
                  disabled={isMutating}
                  onclick={() => openIrrelevant(inv)}
                  title="Nem releváns (indoklás kötelező)"
                >
                  Nem releváns…
                </button>
              {/if}
            </td>
          </tr>
        {/each}
      {/if}
    </tbody>
  </table>
</section>

<dialog
  bind:this={irrelevantDialogEl}
  class="irrelevant-dialog"
  onclose={() => {
    if (irrelevantTarget !== null) {
      // The dialog can close via ESC; reset state regardless.
      irrelevantTarget = null;
      irrelevantReason = "";
      irrelevantError = null;
      irrelevantSubmitting = false;
    }
  }}
  aria-label="Bejövő számla nem releváns megjelölése"
>
  {#if irrelevantTarget !== null}
    <form class="irrelevant-form" onsubmit={submitIrrelevant}>
      <h3 class="irrelevant-title">Nem releváns / Mark as irrelevant</h3>
      <p class="irrelevant-subtitle mono">
        {irrelevantTarget.supplier_name} ·
        {irrelevantTarget.nav_invoice_number}
      </p>
      <label class="irrelevant-row">
        <span class="irrelevant-label">
          Indok / Reason
          <span class="required-marker" aria-hidden="true">*</span>
        </span>
        <textarea
          bind:value={irrelevantReason}
          required
          rows="3"
          placeholder="pl. duplikált tétel, hibás címzett, teszt-számla…"
          disabled={irrelevantSubmitting}
        ></textarea>
      </label>
      {#if irrelevantError !== null}
        <p class="error" role="alert">{irrelevantError}</p>
      {/if}
      <div class="irrelevant-actions">
        <button
          type="button"
          class="quiet-button"
          onclick={closeIrrelevant}
          disabled={irrelevantSubmitting}
        >
          Mégsem
        </button>
        <button
          type="submit"
          class="quiet-button primary"
          disabled={irrelevantSubmitting ||
            !isIrrelevantReasonValid(irrelevantReason)}
        >
          {irrelevantSubmitting ? "Mentés…" : "Megjelölöm nem relevánsnak"}
        </button>
      </div>
    </form>
  {/if}
</dialog>

<style>
  .screen {
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .screen-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    margin-bottom: var(--space-3);
    flex-wrap: wrap;
    gap: var(--space-3);
  }

  h2 {
    margin: 0;
    font-size: var(--type-size-xl);
    font-weight: 500;
    color: var(--color-text-strong);
    letter-spacing: 0.02em;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-wrap: wrap;
  }

  .filter {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }

  .filter-label {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
  }

  .filter select,
  .search input {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .search input {
    width: 28ch;
  }

  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    margin: -1px;
    padding: 0;
    border: 0;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    transition: color var(--motion-fade-in);
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quiet-button:disabled {
    opacity: 0.5;
    cursor: progress;
  }

  .quiet-button.primary {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .toast {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-left-width: 3px;
    padding: var(--space-2) var(--space-3);
    margin-bottom: var(--space-3);
    font-size: var(--type-size-sm);
    color: var(--color-text-strong);
  }

  .toast[data-tone="ok"] {
    border-left-color: var(--color-signal-positive);
  }

  .toast[data-tone="error"] {
    border-left-color: var(--color-signal-negative);
  }

  .toast-text {
    margin-right: var(--space-2);
  }

  .toast-en {
    color: var(--color-text-secondary);
  }

  .toast-dismiss {
    margin-left: auto;
    background: none;
    border: none;
    color: var(--color-text-muted);
    cursor: pointer;
    font-size: var(--type-size-md);
  }

  .toast-dismiss:hover {
    color: var(--color-text-strong);
  }

  .error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: var(--space-2) 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  table.dense {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-md);
    background: var(--color-surface-sunken);
  }

  table.dense thead th {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    font-weight: 500;
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  .sort-header {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: inherit;
    text-transform: inherit;
    letter-spacing: inherit;
    text-align: inherit;
    cursor: pointer;
    display: inline-flex;
    align-items: baseline;
    gap: var(--space-1);
  }

  .sort-header.right {
    justify-content: flex-end;
    width: 100%;
  }

  .sort-header:hover,
  .sort-header:focus-visible {
    color: var(--color-text-strong);
  }

  .sort-header:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  .sort-indicator {
    display: inline-block;
    min-width: 0.75em;
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  table.dense tbody td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
  }

  table.dense tbody tr:hover {
    background: var(--color-surface-raised);
  }

  td.mono,
  .mono {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
  }

  .col-num {
    text-align: right;
  }

  .col-status {
    width: 18ch;
  }

  .col-actions {
    width: 38ch;
    white-space: nowrap;
  }

  .supplier-name {
    color: var(--color-text-primary);
    cursor: help;
  }

  .muted-currency {
    color: var(--color-text-muted);
  }

  .empty-row td {
    text-align: center;
    color: var(--color-text-secondary);
    padding: var(--space-5);
  }

  .empty-hu {
    color: var(--color-text-strong);
    margin: 0 0 var(--space-2) 0;
  }

  .empty-en {
    color: var(--color-text-secondary);
    margin: 0;
    font-size: var(--type-size-sm);
  }

  .clear-filters {
    margin-top: var(--space-3);
  }

  /* Status chip — colour + glyph + label per ADR-0017 §"Adversarial #4".
   * The CSS class suffix maps 1:1 to `IncomingInvoiceStatus`. */
  .status-chip {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 2px var(--space-2);
    border: 1px solid var(--color-surface-divider);
    border-radius: 999px;
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    font-family: var(--type-family-mono);
    cursor: help;
  }

  .status-chip__glyph {
    font-size: var(--type-size-sm);
  }

  .status-chip--outstanding {
    color: var(--color-signal-warning, #c5872d);
    border-color: var(--color-signal-warning, #c5872d);
  }

  .status-chip--paid {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  .status-chip--irrelevant {
    color: var(--color-text-muted);
    border-color: var(--color-text-muted);
  }

  .status-chip--unknown {
    color: var(--color-text-muted);
    border-style: dashed;
  }

  .row-action {
    background: none;
    border: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    padding: 2px var(--space-2);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-body);
    cursor: pointer;
    margin-right: var(--space-1);
  }

  .row-action:hover:not(:disabled) {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .row-action:disabled {
    opacity: 0.5;
    cursor: progress;
  }

  /* Irrelevant-reason modal — same posture as the AR mark-as-paid
   * dialog in InvoiceDetail.svelte; intentionally compact. */
  .irrelevant-dialog {
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-5);
    min-width: 32ch;
  }

  .irrelevant-dialog::backdrop {
    background: rgba(0, 0, 0, 0.45);
  }

  .irrelevant-title {
    margin: 0 0 var(--space-1) 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
  }

  .irrelevant-subtitle {
    margin: 0 0 var(--space-3) 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }

  .irrelevant-row {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-bottom: var(--space-3);
  }

  .irrelevant-label {
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
  }

  .required-marker {
    color: var(--color-signal-negative);
  }

  .irrelevant-form textarea {
    width: 100%;
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    resize: vertical;
  }

  .irrelevant-actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }
</style>
