<script lang="ts">
  // First dense-table screen — pins the table CSS pattern per
  // ADR-0017. Subsequent screens (invoice detail, audit drill-down,
  // billing summary) inherit this style without re-inventing tokens.
  //
  // Columns:
  //   Invoice id          monospace, primary text; PR-25 makes the id
  //                       a clickable inspector affordance — opens
  //                       the InvoiceDetail modal for that invoice.
  //   Series #            monospace, tabular numbers, right-aligned
  //   Fiscal year         monospace, tabular numbers, right-aligned
  //   State               signal-coloured pill (categorical signal)
  //                       — PR-24 / ADR-0036 §7: chip carries icon +
  //                       label + hover tooltip; eleven labels per
  //                       ADR-0036 §2; per-label affordances come
  //                       from `../lib/labels`. PR-31 / session-35
  //                       appends a small `↘` chain-link badge to
  //                       the right of the chip when the row's
  //                       `has_chain_children` flag is true (the
  //                       invoice is the base of at least one
  //                       storno or amendment chain entry) so an
  //                       inspector can see at a glance which rows
  //                       have a chain history without opening the
  //                       detail modal.
  //   Total (gross, HUF)  monospace, tabular numbers, right-aligned
  //
  // Per ADR-0017 §3 every numeric column is monospace + tabular +
  // right-aligned. Per ADR-0017 §1-2 chrome is quiet, colour means
  // state. Per ADR-0017 §"Adversarial review #4" categorical signal
  // is never carried by colour alone — every chip pairs colour with
  // a glyph + label. Per §4 a freshly-fetched table fades in over
  // 200ms — no spinners, no skeleton shimmers.

  import { onDestroy, onMount } from "svelte";
  import {
    cancelInvoiceStorno,
    downloadInvoicePdf,
    listInvoices,
    parseNavUpstreamFault,
    submitInvoice,
    type InvoiceListItem,
    type InvoiceState,
  } from "../lib/api";
  import {
    LIFECYCLE_ORDER,
    labelMeta,
    lifecycleIndex,
    type LabelSignal,
  } from "../lib/labels";
  import { formatTotal, filenameForInvoice } from "../lib/format";
  import type { BankAccountSnapshot, Currency } from "../lib/api";
  import {
    buyerColumnDisplay,
    quickActionMeta,
    quickActionsForState,
    type RowQuickAction,
  } from "../lib/invoice-list";
  // PR-68 / session-90 — keyboard navigation Tier-1 UX lift.
  // `/` focuses the search box, j/k walk rows, Enter opens the
  // focused row's detail, `g g`/`G` jump to top/bottom, `?` toggles
  // the keyboard hints footer. The pure-module helper lives in
  // `../lib/keyboard-nav.ts`; this file wires its closed-vocab
  // `Hotkey` output to the per-list state.
  import {
    filterInvoicesByNeedle,
    makeHotkeyParserState,
    nextRowIndex,
    parseHotkey,
  } from "../lib/keyboard-nav";
  import { navigateTo } from "../lib/router";
  import InvoiceDetail from "./InvoiceDetail.svelte";
  import ModificationInvoice from "./ModificationInvoice.svelte";

  // PR-87 / session-112 — sessionStorage key the new `#/invoices-new`
  // route stashes the just-issued invoice id under, so that on
  // navigation back to `#/invoices` this list can seed its `navStack`
  // and open the detail modal on the just-issued invoice (matching
  // the pre-PR-87 modal-flow's auto-open-detail affordance). Mirror
  // of the `JUST_ISSUED_KEY` in App.svelte — kept duplicated rather
  // than centralised because the constant is read in two places and
  // the duplication keeps each call site self-contained (CLAUDE.md
  // rule 2 — no new module for a single string).
  const JUST_ISSUED_KEY = "aberp:just-issued-invoice-id";

  let rows: InvoiceListItem[] = $state([]);
  let loadState: "idle" | "loading" | "loaded" | "error" = $state("idle");
  let errorMessage: string | null = $state(null);

  // Filter dropdown — "All" plus one entry per label. Defaults to
  // "All" so the first paint matches the pre-PR-24 behaviour.
  let filterLabel: "All" | InvoiceState = $state("All");

  // PR-25 / session-29 — selected invoice id drives the detail modal
  // (`null` keeps it closed; a string opens it and triggers the
  // fetch). Per the session-28 handoff lean: modal / in-place over a
  // routed `/invoice/<id>` URL — no SvelteKit dependency added.
  //
  // PR-30 / session-34 — navigation history stack. Each chain-link
  // traversal in `InvoiceDetail.svelte` pushes the base invoice id
  // onto this stack; the modal shows the top of the stack as the
  // current invoice and renders the entries below it as a
  // breadcrumb of `← {id}` back-buttons. Empty stack keeps the
  // modal closed. ESC / backdrop close clears the entire stack
  // (matches the modal-as-single-inspection-context posture from
  // PR-26 / PR-27 / PR-29). Reassignment pattern (build a new
  // array on every mutation) guarantees Svelte 5 reactivity
  // without depending on Array-mutation tracking through the
  // $state proxy.
  let navStack: string[] = $state([]);

  // PR-87 / session-112 — issue-invoice is now a full-page route
  // (`#/invoices-new`), not a modal mounted here. The "+ New invoice"
  // button below navigates via `navigateTo("invoices-new")`; the
  // route's onIssued callback (in App.svelte) stashes the just-issued
  // id in sessionStorage and navigates back here; `onMount` below
  // reads the stash and seeds `navStack` to open the detail modal on
  // the just-issued invoice (the same auto-open-detail affordance
  // the pre-PR-87 modal flow provided, just bridged across the route
  // transition).

  // PR-47β / session-65 — modification modal state. Holds the base
  // invoice's id, currency, and displayable number while the modal
  // is open; `null` keeps the modal closed. The detail modal's
  // Modification button bubbles its base context up through the
  // `onAmend` callback; on a successful modification the modal
  // invokes `onAmended(newInvoiceId)` and this screen navigates the
  // detail modal to the NEW modification invoice (the operator's
  // regulatory record is the chain child, not the base they amended)
  // + reloads the list.
  let modificationContext: {
    baseInvoiceId: string;
    baseCurrency: Currency;
    baseInvoiceNumber: string;
    /** PR-80 / session-102 — base invoice's bank-account snapshot
     * forwarded into the modify form so it can render the inherited
     * bank readout. `null` for CLI-issued bases (no snapshot). */
    baseBankAccount: BankAccountSnapshot | null;
  } | null = $state(null);

  // PR-65 / session-86 — per-row quick-action state. One row at a
  // time is in-flight (sequential rather than concurrent so the
  // operator's eye tracks the single spinner; a queued second click
  // is no-oped via the `busyRow` gate). On error the row's message
  // surfaces inline below the table — same A157 inline-render
  // posture the detail modal uses; no toast component per
  // CLAUDE.md rule 13. The `actionError` carries the invoice_id so
  // the message lands next to the row that produced it; a fresh
  // click on any row clears the prior error.
  let busyRow: { invoiceId: string; action: RowQuickAction } | null = $state(null);
  let actionError: { invoiceId: string; action: RowQuickAction; message: string } | null =
    $state(null);

  // PR-68 / session-90 — keyboard-nav state. `searchNeedle` drives
  // the client-side substring filter; `focusedRowIndex` tracks the
  // j/k cursor within `visibleRows` (-1 = no row focused yet so the
  // first j/k press parks at row 0 per `nextRowIndex`'s posture).
  // `hintsVisible` toggles the bottom-right hints footer (`?` flips
  // it; closed-vocab `toggle-hints` hotkey). `searchInputEl` is the
  // DOM reference the `/` hotkey focuses; bound via Svelte 5
  // `bind:this`.
  let searchNeedle: string = $state("");
  let focusedRowIndex: number = $state(-1);
  let hintsVisible: boolean = $state(true);
  let searchInputEl: HTMLInputElement | null = $state(null);
  const parserState = makeHotkeyParserState();

  onMount(() => {
    void refresh();
    window.addEventListener("keydown", handleKeydown);
    // PR-87 / session-112 — read the just-issued stash left by the
    // `#/invoices-new` route's onIssued callback, clear it, and seed
    // `navStack` so the detail modal opens on the just-issued
    // invoice. Defensive: missing-stash and storage-API exceptions
    // are silently no-ops (the list still renders fine; the operator
    // just doesn't get the auto-open affordance).
    try {
      const stashed = sessionStorage.getItem(JUST_ISSUED_KEY);
      if (stashed !== null && stashed.length > 0) {
        sessionStorage.removeItem(JUST_ISSUED_KEY);
        navStack = [stashed];
      }
    } catch (_e) {
      // sessionStorage can throw in private-browsing / quota-full
      // contexts; navigation back to the list already succeeded, so
      // swallowing here is the right posture.
    }
  });

  onDestroy(() => {
    window.removeEventListener("keydown", handleKeydown);
  });

  function handleKeydown(event: KeyboardEvent) {
    // The detail modal and modification modal mount inside this
    // component but each manages its own Escape posture. If ANY of
    // those is open, we stand down — modal navigation owns the
    // keyboard while it's visible. PR-87 / session-112 dropped the
    // `issueOpen` gate (IssueInvoice is now its own route; when
    // mounted at `#/invoices-new`, InvoiceList itself is unmounted
    // so this handler isn't wired anyway).
    if (
      navStack.length > 0 ||
      modificationContext !== null
    ) {
      return;
    }
    const hotkey = parseHotkey(event, parserState);
    if (hotkey === null) return;
    switch (hotkey.kind) {
      case "focus-search":
        event.preventDefault();
        searchInputEl?.focus();
        searchInputEl?.select();
        return;
      case "blur-or-clear":
        // Only act when the search input is the one focused — other
        // INPUT targets (e.g. the modal-mounted form fields, if any
        // ever bubble) keep their native behaviour.
        if (event.target === searchInputEl) {
          if (searchNeedle.length > 0) {
            searchNeedle = "";
          } else {
            searchInputEl?.blur();
          }
        }
        return;
      case "row-down":
        event.preventDefault();
        focusedRowIndex = nextRowIndex(focusedRowIndex, 1, visibleRows.length);
        return;
      case "row-up":
        event.preventDefault();
        focusedRowIndex = nextRowIndex(focusedRowIndex, -1, visibleRows.length);
        return;
      case "row-top":
        event.preventDefault();
        focusedRowIndex = visibleRows.length > 0 ? 0 : -1;
        return;
      case "row-bottom":
        event.preventDefault();
        focusedRowIndex = visibleRows.length > 0 ? visibleRows.length - 1 : -1;
        return;
      case "row-open":
        if (focusedRowIndex >= 0 && focusedRowIndex < visibleRows.length) {
          event.preventDefault();
          navStack = [visibleRows[focusedRowIndex].invoice_id];
        }
        return;
      case "toggle-hints":
        event.preventDefault();
        hintsVisible = !hintsVisible;
        return;
    }
  }

  async function refresh() {
    loadState = "loading";
    errorMessage = null;
    try {
      rows = await listInvoices();
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      errorMessage = err instanceof Error ? err.message : String(err);
    }
  }

  // PR-65 / session-86 — per-row quick-action dispatch. Mirrors
  // each detail-modal handler 1:1 (`triggerDownload` /
  // `triggerSubmit` / `triggerCancelStorno` in `InvoiceDetail.svelte`)
  // so the row-level path hits the same Tauri command, the same
  // backend route, the same A157 inline-error rendering, and the
  // same audit-ledger sequence. The detail-modal path stays
  // available for the operator who wants to see the audit trail
  // before clicking; the row-level path is the "one click from the
  // list" lift the brief named. A successful action refreshes the
  // list so the state chip flips without re-mounting.
  //
  // The Storno path keeps the same `window.confirm` gate the modal
  // uses — a stray row click MUST NOT burn a sequence number per
  // ADR-0023 §3.

  async function triggerRowDownload(row: InvoiceListItem) {
    actionError = null;
    busyRow = { invoiceId: row.invoice_id, action: "Download" };
    try {
      const blob = await downloadInvoicePdf(row.invoice_id);
      const composedNumber = `${row.fiscal_year}-${String(row.sequence_number).padStart(6, "0")}`;
      const filename = filenameForInvoice(composedNumber);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = filename;
      document.body.appendChild(anchor);
      anchor.click();
      document.body.removeChild(anchor);
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    } catch (err: unknown) {
      actionError = {
        invoiceId: row.invoice_id,
        action: "Download",
        message: err instanceof Error ? err.message : String(err),
      };
    } finally {
      busyRow = null;
    }
  }

  async function triggerRowSubmit(row: InvoiceListItem) {
    actionError = null;
    busyRow = { invoiceId: row.invoice_id, action: "Submit" };
    try {
      await submitInvoice(row.invoice_id);
      // Refresh so the row's state chip flips to Submitted (and the
      // quick-action button set switches from Submit → PollAck which
      // is detail-modal-only; the row's actions column thins to PDF
      // only on the next render).
      await refresh();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      // parseNavUpstreamFault returns null for non-NAV errors; the
      // raw message is the load-bearing surface for inline render.
      // The row-level UX intentionally does not unpack the typed
      // technicalValidations array — that surface belongs to the
      // detail modal where the operator already has the audit
      // trail context.
      parseNavUpstreamFault(message);
      actionError = { invoiceId: row.invoice_id, action: "Submit", message };
    } finally {
      busyRow = null;
    }
  }

  async function triggerRowStorno(row: InvoiceListItem) {
    actionError = null;
    const number = `${row.fiscal_year}-${String(row.sequence_number).padStart(6, "0")}`;
    const ok = window.confirm(
      `Cancel invoice ${number}?\n\n` +
        `A storno will be issued in the same currency at the same exchange rate. ` +
        `This carves a fresh sequence number and writes three audit-ledger rows; ` +
        `it cannot be reversed.`,
    );
    if (!ok) return;
    busyRow = { invoiceId: row.invoice_id, action: "Storno" };
    try {
      await cancelInvoiceStorno(row.invoice_id);
      await refresh();
    } catch (err: unknown) {
      actionError = {
        invoiceId: row.invoice_id,
        action: "Storno",
        message: err instanceof Error ? err.message : String(err),
      };
    } finally {
      busyRow = null;
    }
  }

  function dispatchQuickAction(row: InvoiceListItem, action: RowQuickAction) {
    if (busyRow !== null) return; // one in-flight at a time
    switch (action) {
      case "Download":
        void triggerRowDownload(row);
        return;
      case "Submit":
        void triggerRowSubmit(row);
        return;
      case "Pay":
        // PR-70 / ADR-0039 — the mark-as-paid form has four fields;
        // duplicating it at the row level would diverge two surfaces
        // (CLAUDE.md rule 7). Open the detail modal so the operator
        // fills the form there. The detail modal renders the
        // "Mark as paid" button on Finalized-and-unpaid invoices
        // (mirror of `quickActionsForState` at the modal level).
        navStack = [row.invoice_id];
        return;
      case "Storno":
        void triggerRowStorno(row);
        return;
    }
  }

  // PR-44ε / session-53 — currency-aware total formatter lives in
  // `../lib/format`. The pre-PR-44ε inline `hufFormatter` +
  // `formatHuf` pair is removed because (a) the SPA now needs the
  // EUR branch (an EUR invoice's `total_gross` is in cents, not
  // forints), and (b) the same logic was duplicated in
  // InvoiceDetail.svelte. The shared module is pinned by
  // `format.test.ts`.

  function signalClass(signal: LabelSignal): string {
    return `signal-${signal}`;
  }

  // Filter + lifecycle-natural sort. Per ADR-0036 §3 the operator's
  // mental model walks Unknown → Ready → Pending → PendingNavExists
  // → Submitted → Recovered → Finalized → Rejected → Storno →
  // Amended → Abandoned; that ordering mirrors the audit-ledger
  // ladder in `serve.rs::derive_state`. Within a bucket, secondary
  // sort by invoice id keeps the display stable across refreshes.
  let visibleRows = $derived(
    filterInvoicesByNeedle(
      rows.filter((r) => filterLabel === "All" || r.state === filterLabel),
      searchNeedle,
    )
      .slice()
      .sort((a, b) => {
        const dx = lifecycleIndex(a.state) - lifecycleIndex(b.state);
        if (dx !== 0) return dx;
        return a.invoice_id.localeCompare(b.invoice_id);
      }),
  );

  // PR-68 / session-90 — keep `focusedRowIndex` valid as the filtered
  // list shrinks. If the operator narrows the search to fewer rows
  // than the prior focus index, clamp to the new bottom; if the list
  // becomes empty, drop to -1.
  $effect(() => {
    if (visibleRows.length === 0) {
      focusedRowIndex = -1;
    } else if (focusedRowIndex >= visibleRows.length) {
      focusedRowIndex = visibleRows.length - 1;
    }
  });
</script>

<section class="screen">
  <div class="screen-head">
    <h2>Invoices</h2>
    <div class="actions">
      <!-- PR-68 / session-90 — substring search across invoice number,
           ULID, buyer name, and state. `/` focuses this input from
           anywhere on the page. -->
      <label class="search">
        <span class="visually-hidden">Search invoices</span>
        <input
          bind:this={searchInputEl}
          bind:value={searchNeedle}
          type="search"
          placeholder="Search by number, buyer, state… (press /)"
          autocomplete="off"
          spellcheck="false"
          aria-label="Search invoices by number, buyer, or state"
        />
      </label>
      <label class="filter">
        <span class="filter-label">State</span>
        <select
          bind:value={filterLabel}
          aria-label="Filter invoices by state"
        >
          <option value="All">All</option>
          {#each LIFECYCLE_ORDER as state (state)}
            <option value={state}>{state}</option>
          {/each}
        </select>
      </label>
      <button
        type="button"
        class="quiet-button"
        onclick={() => void refresh()}
        disabled={loadState === "loading"}
      >
        Refresh
      </button>
      <button
        type="button"
        class="quiet-button primary"
        onclick={() => navigateTo("invoices-new")}
      >
        + New invoice
      </button>
    </div>
  </div>

  {#if loadState === "error"}
    <p class="error" role="alert">{errorMessage}</p>
  {/if}

  <table class="dense">
    <thead>
      <tr>
        <th scope="col" class="col-id">Invoice id</th>
        <!-- PR-65 / session-86 — Partner column. Positioned between
             Invoice id and the numeric Series # / Fiscal year columns
             so the operator's left-to-right scan answers "who was
             this for?" before "which series number?". -->
        <th scope="col" class="col-partner">Partner</th>
        <th scope="col" class="col-num">Series #</th>
        <th scope="col" class="col-num">Fiscal year</th>
        <th scope="col" class="col-state">State</th>
        <th scope="col" class="col-num">Total (gross)</th>
        <!-- PR-65 / session-86 — Actions column. Per-row quick-action
             buttons gated by `quickActionsForState`; empty for
             terminal states with only Download (still rendered, kept
             stable column shape). -->
        <th scope="col" class="col-actions">Actions</th>
      </tr>
    </thead>
    <tbody>
      {#if loadState === "loaded" && visibleRows.length === 0}
        <tr class="empty">
          <td colspan="7">
            {#if rows.length === 0}
              No invoices on this tenant yet. Issue one with
              <code>aberp issue-invoice</code> and reload.
            {:else if searchNeedle.trim().length > 0}
              No invoices match the search
              <code>{searchNeedle}</code>{filterLabel !== "All"
                ? ` and the state filter ${filterLabel}.`
                : "."}
            {:else}
              No invoices match the filter
              <code>{filterLabel}</code>.
            {/if}
          </td>
        </tr>
      {/if}
      {#each visibleRows as row, rowIndex (row.invoice_id)}
        {@const meta = labelMeta(row.state)}
        {@const partnerLabel = buyerColumnDisplay(row.buyer_name)}
        {@const isPartnerMissing = row.buyer_name === null || row.buyer_name.trim().length === 0}
        {@const actions = quickActionsForState(row.state, row.payment !== null)}
        {@const isKeyboardFocused = rowIndex === focusedRowIndex}
        <tr class:row-focused={isKeyboardFocused}>
          <td class="col-id mono">
            <button
              type="button"
              class="id-link"
              onclick={() => (navStack = [row.invoice_id])}
              aria-label={`Open detail for invoice ${row.invoice_id}`}
            >
              {row.invoice_id}
            </button>
          </td>
          <td class="col-partner" class:partner-missing={isPartnerMissing}>
            {partnerLabel}
          </td>
          <td class="col-num mono">{row.sequence_number}</td>
          <td class="col-num mono">{row.fiscal_year}</td>
          <td class="col-state">
            <span
              class="state-pill {signalClass(meta.signal)}"
              title={meta.tooltip}
            >
              <span class="state-icon" aria-hidden="true">{meta.icon}</span>
              <span class="state-text">{row.state}</span>
            </span>
            {#if row.has_chain_children}
              <span
                class="chain-badge"
                aria-label="This invoice is the base of a storno or amendment chain"
                title="This invoice is the base of a storno or amendment chain — open the row to inspect."
              >↘</span>
            {/if}
            {#if row.payment !== null}
              <!-- PR-70 / ADR-0039 §2 — operational Paid badge next to
                   the regulatory state chip. Parallel signal: the
                   state chip continues to render the NAV ladder
                   verbatim (e.g. `Finalized` — the SAVED-ack terminal),
                   the Paid badge announces that an operational payment
                   record exists. -->
              <span
                class="state-pill paid-pill"
                title={`Paid on ${row.payment.paid_at}`}
                aria-label={`Payment recorded on ${row.payment.paid_at}`}
              >
                <span class="state-icon" aria-hidden="true">✓</span>
                <span class="state-text">Paid</span>
              </span>
            {/if}
          </td>
          <td class="col-num mono">{formatTotal(row.total_gross, row.currency)}</td>
          <td class="col-actions">
            <div class="row-actions">
              {#each actions as action (action)}
                {@const meta = quickActionMeta(action)}
                {@const busy =
                  busyRow !== null &&
                  busyRow.invoiceId === row.invoice_id &&
                  busyRow.action === action}
                <button
                  type="button"
                  class="row-action"
                  class:busy
                  disabled={busyRow !== null}
                  onclick={() => dispatchQuickAction(row, action)}
                  aria-label={`${meta.label} — invoice ${row.invoice_id}`}
                  title={meta.label}
                >
                  <span class="row-action-glyph" aria-hidden="true">{meta.glyph}</span>
                  <span class="row-action-label">{meta.label.split(" ")[0]}</span>
                </button>
              {/each}
            </div>
          </td>
        </tr>
      {/each}
    </tbody>
  </table>

  {#if actionError !== null}
    <p class="row-action-error" role="alert">
      {actionError.action} failed for
      <code>{actionError.invoiceId}</code>: {actionError.message}
    </p>
  {/if}

  <!-- PR-68 / session-90 — keyboard hints footer. Quiet aesthetic per
       ADR-0017 §1-2; `?` toggles visibility. -->
  {#if hintsVisible}
    <p class="keyboard-hints" aria-hidden="true">
      Press <kbd>/</kbd> to search • <kbd>j</kbd>/<kbd>k</kbd> to navigate •
      <kbd>Enter</kbd> to open • <kbd>?</kbd> to hide
    </p>
  {/if}

  <InvoiceDetail
    invoiceId={navStack.length > 0 ? navStack[navStack.length - 1] : null}
    ancestors={navStack.slice(0, -1)}
    onClose={() => {
      // PR-88 / session-113 — auto-refresh the list on detail-modal
      // close. The detail modal hosts the Submit, Poll-ack, Storno,
      // and Mark-as-paid actions; each refreshes the modal's own
      // detail (`load(invoice_id)`) but pre-PR-88 NONE refreshed the
      // parent list. The operator saw stale row state (chip not
      // flipped, paid badge missing, sequence number missing on a
      // just-issued chain child) until they manually clicked the
      // (now-obsolete) Refresh button. Refreshing on every close is
      // harmless: a single Tauri invoke + a `<tbody>` re-render is
      // cheap, and the operator's mental model is "modal close ⇒
      // list view is fresh" anyway.
      navStack = [];
      void refresh();
    }}
    onNavigate={(baseId) => (navStack = [...navStack, baseId])}
    onJumpBack={(index) => (navStack = navStack.slice(0, index + 1))}
    onAmend={(baseInvoiceId, baseCurrency, baseInvoiceNumber, baseBankAccount) =>
      (modificationContext = {
        baseInvoiceId,
        baseCurrency,
        baseInvoiceNumber,
        baseBankAccount,
      })}
  />

  <!-- PR-87 / session-112 — IssueInvoice mount removed. Issuance now
       lives at `#/invoices-new` as a full-page route (the "+ New
       invoice" button above navigates there). Post-issuance the
       route navigates back here; this component reads the just-
       issued id from sessionStorage in onMount and seeds navStack
       to open the detail modal — preserving the pre-PR-87 auto-
       open-detail affordance. -->

  <ModificationInvoice
    baseInvoiceId={modificationContext?.baseInvoiceId ?? null}
    baseCurrency={modificationContext?.baseCurrency ?? null}
    baseInvoiceNumber={modificationContext?.baseInvoiceNumber ?? null}
    baseBankAccount={modificationContext?.baseBankAccount ?? null}
    onClose={() => {
      // PR-88 / session-113 — same auto-refresh discipline as the
      // detail modal: closing the modification flow should leave the
      // list in a fresh state. Even on a CANCEL the refresh is
      // harmless; the operator's mental model is "modal close ⇒
      // list is fresh."
      modificationContext = null;
      void refresh();
    }}
    onAmended={(newInvoiceId) => {
      // PR-47β — close the modification modal + refresh the list +
      // navigate the detail modal to the NEW modification invoice
      // (the chain child IS the operator's regulatory record for
      // this amendment; the base flips to `Amended` automatically
      // on the next list refresh via `derive_state`'s
      // `is_amended_base` arm).
      modificationContext = null;
      void refresh();
      navStack = [newInvoiceId];
    }}
  />
</section>

<style>
  .screen {
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .screen-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    margin-bottom: var(--space-3);
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

  .filter select {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .filter select:hover {
    border-color: var(--color-text-muted);
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

  /* PR-44ζ / session-59 — quiet-emphasis primary button for the
   * "+ New invoice" affordance. Slightly stronger than the bare
   * quiet button so the operator's eye is drawn to the issuance
   * action without breaking the ADR-0017 quiet-chrome posture
   * (no accent colour, no fill — just a stronger border + label). */
  .quiet-button.primary {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: var(--space-2) 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  /* Dense table — the load-bearing CSS of ADR-0017. */
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

  table.dense tbody td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
  }

  table.dense tbody tr:hover {
    background: var(--color-surface-raised);
  }

  /* Tabular figures for every numeric column — ADR-0017 §3. */
  td.mono,
  .mono {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
  }

  .col-num {
    text-align: right;
  }

  .col-id {
    width: 30ch;
  }

  /* PR-25 — invoice-id cell is a clickable inspector affordance.
   * Reset the native button chrome; visually it reads as a quiet
   * link so the dense-table aesthetic per ADR-0017 is preserved. */
  .id-link {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: var(--color-text-primary);
    text-align: left;
    cursor: pointer;
  }

  .id-link:hover,
  .id-link:focus-visible {
    color: var(--color-text-strong);
    text-decoration: underline;
    text-decoration-color: var(--color-text-muted);
    text-underline-offset: 2px;
  }

  .id-link:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  /* Widened from 14ch to 22ch — PendingNavExists (16 chars) plus
   * icon + gap is the longest chip; the column must fit without
   * wrapping. PR-31 / session-35 — the chain-link badge appends a
   * single glyph (`↘`) with a small left margin to the cell; the
   * 22ch floor still fits the badge alongside the longest chip
   * because PendingNavExists is the only state that pairs with a
   * chain-children flag rarely (the typical chain-base state is
   * Storno or Amended, both shorter). */
  .col-state {
    width: 22ch;
  }

  /* PR-31 / session-35 — chain-link badge next to the state chip.
   * Quiet aesthetic per ADR-0017 §1-2: muted text, no border,
   * cursor-help to mirror the state pill's hover-tooltip
   * convention. Categorical signal is the glyph itself per
   * ADR-0017 §"Adversarial review #4" — colour is not the
   * load-bearing signal. */
  .chain-badge {
    margin-left: var(--space-1);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    color: var(--color-text-muted);
    cursor: help;
  }

  .state-pill {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 0 var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    line-height: 1.6;
    letter-spacing: 0.04em;
    border: 1px solid var(--color-surface-divider);
    border-radius: 2px;
    background: var(--color-surface-base);
    color: var(--color-text-secondary);
    cursor: help;
  }

  .state-icon {
    /* Glyphs render slightly sharper in the body face than the
     * mono face at small sizes; the chip's overall mono character
     * is preserved by the label text + tabular alignment. */
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    line-height: 1;
  }

  /* Categorical signal colours — only state cells carry colour. */
  .state-pill.signal-positive {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }
  .state-pill.signal-negative {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .state-pill.signal-warning {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  /* PR-24 — reserved violet per ADR-0017 §5. Surfaces ABERP↔NAV
   * record disagreement at the label level; `PendingNavExists` is
   * the exact operator-visible case. */
  .state-pill.signal-divergence {
    color: var(--color-signal-divergence);
    border-color: var(--color-signal-divergence);
  }
  .state-pill.signal-muted {
    color: var(--color-text-muted);
    border-color: var(--color-surface-divider);
  }

  /* PR-70 / ADR-0039 §2 — Paid badge. Sits next to the regulatory
     state chip on the row. Uses the positive signal colour but is
     a separate class from `signal-positive` because paid-vs-unpaid
     is OFF the NAV regulatory ladder; the SAVED chip and Paid chip
     can render side-by-side and need distinguishable signals. */
  .state-pill.paid-pill {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
    margin-left: var(--space-1);
  }

  .empty td {
    color: var(--color-text-muted);
    font-style: italic;
    text-align: center;
    padding: var(--space-5) var(--space-3);
  }

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  /* PR-65 / session-86 — Partner column. Body font (not mono — the
   * buyer name is operator-meaningful prose, not a tabular value)
   * with a width floor wide enough for typical Hungarian legal
   * names without horizontal scroll. The muted variant carries the
   * em-dash placeholder for invoices with no side-store snapshot
   * (CLI-issued, pre-PR-47α) so the operator's eye reads "no value"
   * the same way it reads other empty signal slots elsewhere in the
   * ADR-0017 vocabulary. */
  .col-partner {
    width: 28ch;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  .col-partner.partner-missing {
    color: var(--color-text-muted);
  }

  /* PR-65 / session-86 — Actions column. Quiet per-row buttons with
   * an icon-glyph + short label. Per ADR-0017 §1-2 the chrome stays
   * quiet (no accent fill); per ADR-0017 §"Adversarial review #4"
   * the glyph carries the categorical signal in addition to the
   * label, so a colour-blind operator still distinguishes Download
   * from Submit from Storno.
   *
   * The `disabled` posture (busyRow !== null) prevents a second
   * concurrent click while one is in-flight — the spinner indicator
   * is the `.busy` class on the active row's button. */
  .col-actions {
    width: 22ch;
    text-align: right;
  }

  .row-actions {
    display: inline-flex;
    gap: var(--space-1);
    justify-content: flex-end;
  }

  .row-action {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: 0 var(--space-2);
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    line-height: 1.8;
    cursor: pointer;
    transition: color var(--motion-fade-in);
  }

  .row-action:hover:not(:disabled) {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .row-action:disabled {
    opacity: 0.5;
    cursor: progress;
  }

  /* Visual cue for the in-flight row's button. The cursor flips to
   * `progress` and the glyph dims; the parent's `disabled` state
   * already prevents re-click. */
  .row-action.busy {
    color: var(--color-text-muted);
  }

  .row-action-glyph {
    font-size: var(--type-size-sm);
    line-height: 1;
  }

  .row-action-error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: var(--space-2) 0 0 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  /* PR-68 / session-90 — `/`-targeted search input. Matches the
   * PartnersList screen's `.page__search input` posture so the two
   * list views feel like siblings. */
  .search input {
    width: 320px;
    max-width: 100%;
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .search input::placeholder {
    color: var(--color-text-muted);
  }

  .search input:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 1px;
  }

  /* PR-68 / session-90 — focused-row highlight for the j/k cursor.
   * Subtle outline + slight surface lift; categorical signal is the
   * outline (not colour alone) per ADR-0017 §"Adversarial review #4".
   * The hover background still wins on cursor hover so the operator
   * can keep using the mouse without the keyboard cursor stealing
   * the cue. */
  table.dense tbody tr.row-focused {
    background: var(--color-surface-raised);
    outline: 1px solid var(--color-text-muted);
    outline-offset: -1px;
  }

  .keyboard-hints {
    margin: var(--space-3) 0 0 0;
    text-align: right;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-body);
  }

  .keyboard-hints kbd {
    display: inline-block;
    padding: 0 var(--space-1);
    border: 1px solid var(--color-surface-divider);
    border-radius: 2px;
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    line-height: 1.4;
  }

  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }
</style>
