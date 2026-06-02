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
    downloadInvoicePdf,
    listInvoices,
    parseNavUpstreamFault,
    pollAck,
    submitInvoice,
    type InvoiceListItem,
    type InvoiceState,
  } from "../lib/api";
  import { navStatusPictogram } from "../lib/nav-status-pictogram";
  import {
    LIFECYCLE_ORDER,
    labelMeta,
    lifecycleIndex,
    type LabelSignal,
  } from "../lib/labels";
  import { formatInvoiceTotal, filenameForInvoice } from "../lib/format";
  import type { BankAccountSnapshot, Currency, RowKind } from "../lib/api";
  import {
    EMPTY_FILTER,
    buyerColumnDisplay,
    compareInvoices,
    filterInvoices,
    isFilterEmpty,
    quickActionMeta,
    quickActionsForState,
    type InvoiceFilterSpec,
    type RowQuickAction,
    type SortDir,
    type SortKey,
  } from "../lib/invoice-list";
  // PR-175 / session-175 — persist sort + filter selection to
  // localStorage so the operator's view survives a reload / app
  // restart. Pure helpers in invoice-list-persistence.ts; this file
  // just reads on mount and writes on every mutation.
  import {
    loadInvoiceListPrefs,
    saveInvoiceListPrefs,
  } from "../lib/invoice-list-persistence";
  // PR-193 / session-193 — CSV export of the currently-displayed
  // (filtered + sorted) row set. Tier-4 "invisible excellence" lift
  // for the bookkeeper who wants this view in their spreadsheet.
  import {
    composeCsv,
    csvFilenameTimestamp,
    downloadCsv,
  } from "../lib/csv-export";
  // PR-68 / session-90 — keyboard navigation Tier-1 UX lift.
  // `/` focuses the search box, j/k walk rows, Enter opens the
  // focused row's detail, `g g`/`G` jump to top/bottom, `?` toggles
  // the keyboard hints footer. The pure-module helper lives in
  // `../lib/keyboard-nav.ts`; this file wires its closed-vocab
  // `Hotkey` output to the per-list state.
  import {
    makeHotkeyParserState,
    nextRowIndex,
    parseHotkey,
  } from "../lib/keyboard-nav";
  import { navigateTo } from "../lib/router";
  import InvoiceDetail from "./InvoiceDetail.svelte";
  import ModificationInvoice from "./ModificationInvoice.svelte";
  // S220 / PR-217 — operator-paced partner picker for ExtNav rows.
  // NAV does not expose buyer info for invoices submitted via other
  // software, so the operator needs an affordance to annotate the
  // row from their own records.
  import ExtNavPartnerPickerModal from "../lib/ExtNavPartnerPickerModal.svelte";

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

  // PR-175 / session-175 — seed both `filter` and `sort` from
  // `localStorage` so a reload restores the operator's last view.
  // `loadInvoiceListPrefs` returns the default blob on every failure
  // path (key absent, malformed JSON, unknown vocab from a future
  // schema), so the legacy "open filter + lifecycle-natural sort"
  // posture is the safe fallback in every case.
  const initialPrefs = loadInvoiceListPrefs();

  // PR-94 / session-114 — unified filter spec (needle + state +
  // currency facets). The `needle` field drives the PR-68 `/`-targeted
  // substring search; the `state` and `currency` fields drive the new
  // facet dropdowns next to it. All three AND together; an "All" value
  // on a facet short-circuits the gate. The previous standalone
  // `filterLabel` + `searchNeedle` reactive fields are subsumed here so
  // one object drives every filter surface.
  let filter: InvoiceFilterSpec = $state({ ...initialPrefs.filter });

  // PR-94 / session-114 — sortable-columns state. `key === null` keeps
  // the legacy lifecycle-natural ordering (Unknown → Abandoned, then
  // invoice_id ascending). Clicking a column header three-cycles:
  // null → asc → desc → null (reset). Clicking a different column
  // jumps to (clicked, asc) directly. The renderer shows ▲ / ▼ next
  // to the active column label.
  let sort: { key: SortKey | null; dir: SortDir } = $state({ ...initialPrefs.sort });

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

  // ADR-0049 §Initiation (session 156) — one-shot flag passed to the
  // detail modal so the row quick-action's Storno opens the modal's
  // inline confirm panel (the canonical confirm+reason surface) instead
  // of the Tauri-unreliable `window.confirm` the row path used to fire.
  // Reset on modal close + on chain-navigation so a normal "open to
  // inspect" never auto-opens the panel.
  let stornoArmOnOpen: boolean = $state(false);

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

  // PR-95 / session-115 — per-row pictogram-poll state. Independent
  // of `busyRow` because the pictogram-poll lives in the State
  // column, not the Actions column, AND its busy gate must not
  // disable the Actions buttons (the operator can still download /
  // submit a different row while a poll is in flight). Tracks which
  // invoice id is currently being re-polled so the renderer can
  // swap the pictogram glyph to a spinner-ish "…" affordance.
  let busyPictogramRow: string | null = $state(null);
  let pictogramError: { invoiceId: string; message: string } | null = $state(null);

  // PR-68 / session-90 — keyboard-nav state. The needle now lives on
  // `filter.needle` (PR-94 unified the filter object); `focusedRowIndex`
  // tracks the j/k cursor within `visibleRows` (-1 = no row focused
  // yet so the first j/k press parks at row 0 per `nextRowIndex`'s
  // posture). `hintsVisible` toggles the bottom-right hints footer
  // (`?` flips it; closed-vocab `toggle-hints` hotkey). `searchInputEl`
  // is the DOM reference the `/` hotkey focuses; bound via Svelte 5
  // `bind:this`.
  let focusedRowIndex: number = $state(-1);
  let hintsVisible: boolean = $state(true);
  let searchInputEl: HTMLInputElement | null = $state(null);
  const parserState = makeHotkeyParserState();

  // S220 / PR-217 — partner-picker modal state. `null` = closed; a
  // string id opens the modal for the named restored ExtNav row.
  // Snapshots `buyerName` + `sourceNavInvoiceNumber` at open time so
  // the modal header can render them without a re-lookup against
  // `rows`.
  let pickerRestoredId: string | null = $state(null);
  let pickerBuyerName: string | null = $state(null);
  let pickerSourceNum: string | null = $state(null);

  function openPartnerPicker(row: InvoiceListItem) {
    pickerRestoredId = row.invoice_id;
    pickerBuyerName = row.buyer_name;
    pickerSourceNum = row.source_nav_invoice_number;
  }
  function closePartnerPicker() {
    pickerRestoredId = null;
  }
  function onPartnerPickerUpdated() {
    // Refresh the list so the row's `buyer_name` + sort position
    // reflect the new link. Per [[trust-code-not-operator]] we
    // refresh the WHOLE list rather than patching the one row
    // optimistically — the new buyer label can shift the row in
    // a name-sorted view, and a partial-state SPA after a backend
    // write is the kind of silent-drift the project refuses.
    void refresh();
  }

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
          if (filter.needle.length > 0) {
            filter = { ...filter, needle: "" };
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
        // PR-94 / session-114 — if a <button> (sort header, refresh,
        // +New invoice, clear-filters, row quick-action) is the
        // focused element, the browser's native Enter handler fires
        // the button's click; emitting row-open on top would
        // double-fire (toggle the sort AND open the keyboard-focused
        // row). Suppress here so the button's click is the only
        // action.
        if (
          event.target instanceof HTMLElement &&
          event.target.tagName === "BUTTON"
        ) {
          return;
        }
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
  // The Storno path is the exception (ADR-0049 §Initiation): it does
  // NOT POST from the row. A stray row click MUST NOT burn a sequence
  // number (ADR-0023 §3), so it routes into the detail modal's inline
  // confirm panel — see `triggerRowStorno` below.

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
      // PR-99 Item 3 — auto-poll once the submit returns so the row's
      // pictogram + state chip advance to terminal (Finalized /
      // Rejected) without forcing the operator to click the pictogram
      // a second time. Best-effort: poll failure (network blip, 31s
      // timeout while NAV is still on PROCESSING) leaves the row in
      // Submitted with the pictogram actionable for a manual retry.
      try {
        await pollAck(row.invoice_id);
      } catch (_pollErr) {
        // Tolerated; pictogram stays clickable.
      }
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

  // ADR-0049 §Initiation (session 156) — the row quick-action no longer
  // fires its own `window.confirm` + `cancelInvoiceStorno`. That
  // `window.confirm` gate returns falsy in the Tauri webview (the same
  // unreliability PR-80 abandoned in the modal), so the row path
  // silently did nothing ("storno did not start"). Instead the row
  // action opens the detail modal pre-armed to the inline storno confirm
  // panel — one confirm surface, one reason-capture surface, the path
  // that works today. The actual POST happens in
  // `InvoiceDetail.svelte::triggerConfirmStorno`.
  function triggerRowStorno(row: InvoiceListItem) {
    actionError = null;
    stornoArmOnOpen = true;
    navStack = [row.invoice_id];
  }

  // PR-95 / session-115 — clickable pictogram handler. Invoked when
  // the operator clicks the NAV-status pictogram on a row whose state
  // maps to `InFlight` (the actionable arm of the 4-state vocab —
  // PROCESSING / RECEIVED / Pending / PendingNavExists / Recovered).
  // Same Tauri command the obsoleted Poll-ack button hit; same
  // bounded 31s loop on the backend per ADR-0009 §5. On success
  // refreshes the list so the row's pictogram glyph + state chip
  // flip if NAV returned a terminal ack; on failure surfaces the
  // message inline below the table (same A157 posture).
  //
  // stopPropagation on the click is the renderer's job — clicking
  // the pictogram MUST NOT also bubble up to the row's click handler
  // (which would open the detail modal). The renderer-level
  // `event.stopPropagation()` keeps the surfaces independent.
  async function triggerPictogramPoll(row: InvoiceListItem) {
    if (busyPictogramRow !== null) return;
    pictogramError = null;
    busyPictogramRow = row.invoice_id;
    try {
      await pollAck(row.invoice_id);
      await refresh();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      // parseNavUpstreamFault returns null for non-NAV errors; the raw
      // message is the load-bearing surface for the inline render.
      parseNavUpstreamFault(message);
      pictogramError = { invoiceId: row.invoice_id, message };
    } finally {
      busyPictogramRow = null;
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

  // PR-94 / session-114 — filter → sort composition.
  //
  // 1. `filterInvoices` AND-combines the needle (PR-68), the state
  //    facet, and the currency facet (PR-94). EMPTY_FILTER short-
  //    circuits every gate so the unfiltered display is unchanged.
  // 2. Sort path:
  //    - `sort.key === null` → fall back to the lifecycle-natural
  //      ordering (ADR-0036 §3) the screen has shipped with since
  //      PR-24. Stable: ties go to invoice_id ascending.
  //    - `sort.key !== null` → `compareInvoices` (which itself
  //      tiebreaks on invoice_id ascending regardless of dir).
  let visibleRows = $derived(
    filterInvoices(rows, filter)
      .slice()
      .sort((a, b) => {
        if (sort.key === null) {
          const dx = lifecycleIndex(a.state) - lifecycleIndex(b.state);
          if (dx !== 0) return dx;
          return a.invoice_id.localeCompare(b.invoice_id);
        }
        return compareInvoices(a, b, sort.key, sort.dir);
      }),
  );

  // PR-193 / session-193 — compose the CSV from the currently-displayed
  // (filter + sort applied) row set and trigger a browser download.
  // Columns match what the operator sees on screen plus a couple of
  // bookkeeper-useful additions (composed invoice number, raw amount
  // in major units, currency code). Storno rows negate `total_gross`
  // for display the same way the table cell does — the CSV must
  // match what the operator copied from screen, not the audit-stored
  // positive form. EUR amounts divide by 100 to get major units; HUF
  // passes through (no sub-unit). `null` totals (draft rows that
  // never had a total persisted) render as empty cells.
  function exportCsv() {
    const headers = [
      "Kind",
      "Invoice ID",
      "Invoice number",
      "Partner",
      "Series #",
      "Fiscal year",
      "State",
      "Total gross",
      "Currency",
      "Paid",
    ];
    const rowsOut: unknown[][] = visibleRows.map((row) => {
      // PR-213 / S215 — ExtNav rows carry the raw NAV invoice number
      // as their operator-readable identifier; Own rows carry the
      // composed `YYYY-NNNNNN`. The "Invoice number" CSV column
      // surfaces whichever applies so the bookkeeper's spreadsheet
      // can sort across both kinds without re-encoding.
      const composedNumber =
        row.row_kind === "ExtNav" && row.source_nav_invoice_number !== null
          ? row.source_nav_invoice_number
          : `${row.fiscal_year}-${String(row.sequence_number).padStart(6, "0")}`;
      const partner =
        row.buyer_name === null || row.buyer_name.trim().length === 0
          ? ""
          : row.buyer_name;
      let totalMajor: number | string = "";
      if (row.total_gross !== null) {
        const signed = row.is_storno ? -row.total_gross : row.total_gross;
        totalMajor = row.currency === "EUR" ? signed / 100 : signed;
      }
      return [
        row.row_kind,
        row.invoice_id,
        composedNumber,
        partner,
        row.sequence_number,
        row.fiscal_year,
        row.state,
        totalMajor,
        row.currency,
        row.payment !== null ? "Yes" : "No",
      ];
    });
    const csv = composeCsv(headers, rowsOut);
    const filename = `aberp-invoices-${csvFilenameTimestamp()}.csv`;
    downloadCsv(filename, csv);
  }

  // PR-94 / session-114 — three-click sort cycle for a column header.
  // First click on a column: (column, asc). Second click on the same
  // column: (column, desc). Third click on the same column: reset to
  // the lifecycle-natural default. Clicking a different column always
  // jumps to (clicked, asc) directly — the operator's mental model is
  // "I want to see this column ascending now"; an inherited dir from
  // the previous column would be a footgun.
  function onSortClick(key: SortKey) {
    if (sort.key !== key) {
      sort = { key, dir: "asc" };
      return;
    }
    if (sort.dir === "asc") {
      sort = { key, dir: "desc" };
      return;
    }
    sort = { key: null, dir: "asc" };
  }

  // PR-94 / session-114 — sort-indicator glyph for a column header.
  // `▲` ascending, `▼` descending, empty when the column is not the
  // active sort column (or no sort is set). Glyph is the load-bearing
  // categorical signal per ADR-0017 §"Adversarial review #4" — colour
  // is not the indicator carrier.
  function sortIndicator(key: SortKey): string {
    if (sort.key !== key) return "";
    return sort.dir === "asc" ? "▲" : "▼";
  }

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

  // PR-175 / session-175 — write the operator's current sort + filter
  // selection to localStorage on every change. Reading `sort` and
  // `filter` (including their nested fields) makes this `$effect`
  // re-run on every mutation; `saveInvoiceListPrefs` swallows storage
  // failures internally so a write throw never breaks interaction.
  $effect(() => {
    saveInvoiceListPrefs({
      sort: { key: sort.key, dir: sort.dir },
      filter: {
        needle: filter.needle,
        state: filter.state,
        currency: filter.currency,
        row_kind: filter.row_kind,
      },
    });
  });
</script>

<section class="screen">
  <div class="screen-head">
    <h2>Invoices</h2>
    <div class="actions">
      <!-- PR-68 / session-90 — substring search across invoice number,
           ULID, buyer name, and state. `/` focuses this input from
           anywhere on the page. PR-94 / session-114 — value lives on
           the unified `filter.needle` field. -->
      <label class="search">
        <span class="visually-hidden">Search invoices</span>
        <input
          bind:this={searchInputEl}
          value={filter.needle}
          oninput={(e) =>
            (filter = {
              ...filter,
              needle: (e.currentTarget as HTMLInputElement).value,
            })}
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
          value={filter.state}
          onchange={(e) =>
            (filter = {
              ...filter,
              state: (e.currentTarget as HTMLSelectElement).value as
                | "All"
                | InvoiceState,
            })}
          aria-label="Filter invoices by state"
        >
          <option value="All">All</option>
          {#each LIFECYCLE_ORDER as state (state)}
            <option value={state}>{state}</option>
          {/each}
        </select>
      </label>
      <!-- PR-94 / session-114 — currency facet. Closed-vocab two
           values today (HUF + EUR per ADR-0037 §3); widening to a
           third currency lifts here when `Currency` widens. -->
      <label class="filter">
        <span class="filter-label">Currency</span>
        <select
          value={filter.currency}
          onchange={(e) =>
            (filter = {
              ...filter,
              currency: (e.currentTarget as HTMLSelectElement).value as
                | "All"
                | Currency,
            })}
          aria-label="Filter invoices by currency"
        >
          <option value="All">All</option>
          <option value="HUF">HUF</option>
          <option value="EUR">EUR</option>
        </select>
      </label>
      <!-- PR-213 / S215 — Kind facet. Closed-vocab `RowKind` per
           ADR-0058. Filter to `Own` to hide every NAV-mirror row;
           filter to `ExtNav` to see only the elsewhere-issued
           prefix (operator's YTD reconciliation view). -->
      <label class="filter">
        <span class="filter-label">Kind</span>
        <select
          value={filter.row_kind}
          onchange={(e) =>
            (filter = {
              ...filter,
              row_kind: (e.currentTarget as HTMLSelectElement).value as
                | "All"
                | RowKind,
            })}
          aria-label="Filter invoices by row kind"
        >
          <option value="All">All</option>
          <option value="Own">Own</option>
          <option value="ExtNav">External (NAV mirror)</option>
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
      <!-- PR-193 / session-193 — CSV export of the currently-displayed
           rows (post-filter, post-sort). Disabled when nothing would
           be exported so the operator does not get a 1-line headers-
           only file. -->
      <button
        type="button"
        class="quiet-button"
        onclick={exportCsv}
        disabled={visibleRows.length === 0}
        title="Export the currently displayed rows to a CSV file"
      >
        Export CSV
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
        <!-- PR-213 / S215 — Kind column. Closed-vocab `RowKind` chip
             (`Own` / `Ext.`) sortable like every other column per
             ADR-0058. The Invoice id (UUID) column from PR-25 / PR-94
             is dropped — operators read invoices by `2026-000054`,
             not by ULID; the UUID is still the `#each` key for Svelte
             reactivity but is no longer rendered as a column. -->
        <th
          scope="col"
          class="col-row-kind"
          aria-sort={sort.key === "row_kind"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header"
            onclick={() => onSortClick("row_kind")}
          >
            <span>Kind</span>
            <span class="sort-indicator" aria-hidden="true">{sortIndicator("row_kind")}</span>
          </button>
        </th>
        <!-- PR-65 / session-86 — Partner column. Positioned between
             Invoice id and the numeric Series # / Fiscal year columns
             so the operator's left-to-right scan answers "who was
             this for?" before "which series number?". -->
        <th
          scope="col"
          class="col-partner"
          aria-sort={sort.key === "partner"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header"
            onclick={() => onSortClick("partner")}
          >
            <span>Partner</span>
            <span class="sort-indicator" aria-hidden="true">{sortIndicator("partner")}</span>
          </button>
        </th>
        <th
          scope="col"
          class="col-num"
          aria-sort={sort.key === "series_number"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header right"
            onclick={() => onSortClick("series_number")}
          >
            <span>Series #</span>
            <span class="sort-indicator" aria-hidden="true">{sortIndicator("series_number")}</span>
          </button>
        </th>
        <th
          scope="col"
          class="col-num"
          aria-sort={sort.key === "fiscal_year"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header right"
            onclick={() => onSortClick("fiscal_year")}
          >
            <span>Fiscal year</span>
            <span class="sort-indicator" aria-hidden="true">{sortIndicator("fiscal_year")}</span>
          </button>
        </th>
        <th
          scope="col"
          class="col-state"
          aria-sort={sort.key === "state"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header"
            onclick={() => onSortClick("state")}
          >
            <span>State</span>
            <span class="sort-indicator" aria-hidden="true">{sortIndicator("state")}</span>
          </button>
        </th>
        <th
          scope="col"
          class="col-num"
          aria-sort={sort.key === "total"
            ? sort.dir === "asc"
              ? "ascending"
              : "descending"
            : "none"}
        >
          <button
            type="button"
            class="sort-header right"
            onclick={() => onSortClick("total")}
          >
            <span>Total (gross)</span>
            <span class="sort-indicator" aria-hidden="true">{sortIndicator("total")}</span>
          </button>
        </th>
        <!-- PR-65 / session-86 — Actions column. Per-row quick-action
             buttons gated by `quickActionsForState`; empty for
             terminal states with only Download (still rendered, kept
             stable column shape). Not sortable — actions have no
             natural ordering. -->
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
            {:else}
              <!-- PR-94 / session-114 — facet-aware empty state. The
                   "Clear filters" button resets every facet + the
                   needle in one click; surfaced ONLY when at least
                   one facet is engaged (CLAUDE.md rule 12 — the
                   button has nothing to do when EMPTY_FILTER already
                   holds, so don't tempt the operator with a no-op). -->
              No invoices match the current filters.
              {#if !isFilterEmpty(filter)}
                <button
                  type="button"
                  class="quiet-button clear-filters"
                  onclick={() => (filter = { ...EMPTY_FILTER })}
                >
                  Clear filters
                </button>
              {/if}
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
        {@const pictogram = navStatusPictogram(row.state, row.payment !== null)}
        {@const pictogramBusy = busyPictogramRow === row.invoice_id}
        <tr class:row-focused={isKeyboardFocused}>
          <!-- PR-213 / S215 — Kind chip + click-to-inspect affordance.
               The chip carries the `RowKind` categorical signal (glyph
               + label per ADR-0017 §"Adversarial review #4"); the
               click on the chip opens the detail modal — replacing the
               dropped UUID column's click affordance. For ExtNav rows
               the source NAV invoice number renders as a muted line
               below the chip so the operator has a readable identity
               for non-ABERP-issued rows (the digest does not carry a
               buyer name). -->
          <td class="col-row-kind">
            <button
              type="button"
              class="id-link"
              onclick={() => (navStack = [row.invoice_id])}
              aria-label={`Open detail for invoice ${row.invoice_id}`}
            >
              <span
                class="row-kind-chip"
                class:kind-own={row.row_kind === "Own"}
                class:kind-ext-nav={row.row_kind === "ExtNav"}
                title={row.row_kind === "Own"
                  ? "Canonical ABERP-issued invoice"
                  : "External: NAV mirror — issued under our tax number outside ABERP (e.g. Billingo, manual). Read-only."}
              >
                <span class="row-kind-glyph" aria-hidden="true"
                  >{row.row_kind === "Own" ? "●" : "↺"}</span
                >
                <span class="row-kind-text"
                  >{row.row_kind === "Own" ? "Own" : "Ext."}</span
                >
              </span>
            </button>
            {#if row.row_kind === "ExtNav" && row.source_nav_invoice_number !== null}
              <div class="ext-nav-id mono" title="Raw NAV invoice number">
                {row.source_nav_invoice_number}
              </div>
            {/if}
          </td>
          <td class="col-partner" class:partner-missing={isPartnerMissing}>
            <!-- S220 / PR-217 — ExtNav rows get a click affordance so
                 the operator can link a partner manually. NAV does not
                 expose buyer info for invoices submitted via other
                 software (Billingo / KBoss / etc.) — the boot-time
                 backfill is structurally unable to populate
                 `customer_name` for those rows. Own rows keep the
                 plain label (their buyer rides the side-store JSON
                 at issue time and never needs operator annotation). -->
            {#if row.row_kind === "ExtNav"}
              <button
                type="button"
                class="extnav-partner-link"
                class:extnav-partner-link-empty={isPartnerMissing}
                onclick={() => openPartnerPicker(row)}
                title={isPartnerMissing
                  ? "NAV nem adja meg a vevő adatait külső szoftverrel kiállított számlákhoz. Kattints partner hozzárendeléséhez. / NAV does not expose buyer info for invoices submitted via other software. Click to link a partner manually."
                  : "Kattints a partner módosításához vagy törléséhez. / Click to change or clear the linked partner."}
                aria-label={isPartnerMissing
                  ? "Link a partner to this externally-submitted invoice"
                  : `Change linked partner (currently ${partnerLabel})`}
              >
                {partnerLabel}
              </button>
            {:else}
              {partnerLabel}
            {/if}
          </td>
          <td class="col-num mono">{row.sequence_number}</td>
          <td class="col-num mono">{row.fiscal_year}</td>
          <td class="col-state">
            <!-- PR-213 / S215 — ExtNav rows have no NAV lifecycle from
                 our records (`queryInvoiceDigest` doesn't expose ack
                 status). Render a quiet em-dash placeholder rather than
                 a misleading "Unknown" pictogram + state chip; the
                 operator's eye reads "no value to show here" the same
                 way it reads other empty signal cells. -->
            {#if row.row_kind === "ExtNav"}
              <span class="ext-nav-state-placeholder" aria-label="State not applicable for NAV-mirror row">—</span>
            {:else}
            <!-- PR-95 / session-115 — NAV-status pictogram. 4-state
                 closed vocab; click-to-recheck only on `InFlight`.
                 stopPropagation keeps the row's id-link click
                 (which opens the detail modal) independent of the
                 pictogram click. The button vs span split is
                 ARIA / keyboard discipline: the actionable arm gets
                 the focusable button affordance; the static arms
                 render as plain spans (the row id-link is the
                 keyboard path to inspect those). -->
            {#if pictogram.actionable}
              <button
                type="button"
                class="nav-pictogram {pictogram.kind_class} actionable"
                class:busy={pictogramBusy}
                onclick={(e) => {
                  e.stopPropagation();
                  void triggerPictogramPoll(row);
                }}
                disabled={busyPictogramRow !== null}
                aria-label={pictogram.tooltip_en}
                title={`${pictogram.tooltip_hu} / ${pictogram.tooltip_en}`}
              >
                <span aria-hidden="true">
                  {pictogramBusy ? "…" : pictogram.glyph}
                </span>
              </button>
            {:else}
              <span
                class="nav-pictogram {pictogram.kind_class}"
                aria-label={pictogram.tooltip_en}
                title={`${pictogram.tooltip_hu} / ${pictogram.tooltip_en}`}
              >
                <span aria-hidden="true">{pictogram.glyph}</span>
              </span>
            {/if}
            <!-- Session 162 — a paid invoice collapses to the single
                 bag-of-coins pictogram (above). Ervin's ask: "on paid
                 invoices no need to stack statuses like green check,
                 Finalised, Paid. One final is enough as it supposed to
                 have all priors." Paid is a strict superset of the
                 SAVED-ack `Finalized` state (mark-as-paid is Finalized-
                 gated), so the bag-of-coins implies it — the regulatory
                 state chip + the separate Paid pill are dropped to avoid
                 the stack. The chain badge still renders (it's an
                 orthogonal "this invoice has children" signal). -->
            {#if pictogram.state !== "Paid"}
              <span
                class="state-pill {signalClass(meta.signal)}"
                title={meta.tooltip}
              >
                <span class="state-icon" aria-hidden="true">{meta.icon}</span>
                <span class="state-text">{row.state}</span>
              </span>
            {/if}
            {#if row.has_chain_children}
              <span
                class="chain-badge"
                aria-label="This invoice is the base of a storno or amendment chain"
                title="This invoice is the base of a storno or amendment chain — open the row to inspect."
              >↘</span>
            {/if}
            {#if row.payment !== null && pictogram.state !== "Paid"}
              <!-- PR-70 / ADR-0039 §2 — operational Paid badge next to
                   the regulatory state chip, shown ONLY when the
                   pictogram did NOT already collapse to the Paid
                   bag-of-coins (i.e. a payment recorded against a
                   non-`Final` invoice — a defensive case the pictogram
                   leaves on its base mapping; the pill then still
                   announces the payment record). -->
              <span
                class="state-pill paid-pill"
                title={`Paid on ${row.payment.paid_at}`}
                aria-label={`Payment recorded on ${row.payment.paid_at}`}
              >
                <span class="state-icon" aria-hidden="true">✓</span>
                <span class="state-text">Paid</span>
              </span>
            {/if}
            {/if}
          </td>
          <td class="col-num mono"
            >{formatInvoiceTotal(row.total_gross, row.currency, row.is_storno)}</td
          >
          <td class="col-actions">
            <!-- PR-213 / S215 — ExtNav rows are read-only by construction:
                 NAV-mirror rows belong to whoever issued them outside
                 ABERP. The hard-hide is a component-level invariant per
                 ADR-0058 (NOT a tooltip warning, NOT operator restraint).
                 Without this guard a row-click would hit a backend route
                 that 404s on a non-canonical invoice id (rinv_*) — the
                 component MUST never present the affordance in the first
                 place. -->
            {#if row.row_kind === "Own"}
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
            {/if}
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

  {#if pictogramError !== null}
    <!-- PR-95 / session-115 — pictogram-poll error surface. Mirrors
         the `.row-action-error` posture so the operator finds both
         error classes in the same row beneath the table. -->
    <p class="row-action-error" role="alert">
      NAV ack poll failed for
      <code>{pictogramError.invoiceId}</code>: {pictogramError.message}
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
    openStornoOnLoad={stornoArmOnOpen}
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
      // ADR-0049 §Initiation — disarm the one-shot so a later "open to
      // inspect" never auto-opens the storno panel.
      stornoArmOnOpen = false;
      void refresh();
    }}
    onNavigate={(baseId) => {
      // Chain-navigation moves to a different invoice; disarm so the
      // storno panel does not auto-open on the navigated-to invoice.
      stornoArmOnOpen = false;
      navStack = [...navStack, baseId];
    }}
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

<!-- S220 / PR-217 — partner-picker modal for ExtNav rows. Mounted at
     the screen level so the dialog backdrop covers the table; opens
     when `pickerRestoredId !== null`. -->
<ExtNavPartnerPickerModal
  restoredId={pickerRestoredId}
  currentBuyerName={pickerBuyerName}
  sourceNavInvoiceNumber={pickerSourceNum}
  onUpdated={onPartnerPickerUpdated}
  onClose={closePartnerPicker}
/>

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

  /* PR-94 / session-114 — sortable column header buttons. Reset the
   * native button chrome so they read as plain header text; the only
   * visible signal is the cursor and the trailing ▲ / ▼ glyph. Per
   * ADR-0017 §1-2 (quiet chrome) — no fill, no border. Per
   * ADR-0017 §"Adversarial review #4" — the glyph is the categorical
   * signal, not colour. */
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
    /* Numeric columns are right-aligned per ADR-0017 §3; the
     * sortable header inherits that alignment via this modifier. The
     * `<th>` already has left text-align; we override on the button
     * so the column heading sits flush-right above the values. */
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
    /* Always reserve a slot so the column width does not jitter as
     * the operator clicks between columns. The glyph is a single
     * arrow at the body face; muted text colour matches the inactive
     * header tint per ADR-0017's quiet posture. */
    display: inline-block;
    min-width: 0.75em;
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  /* PR-94 / session-114 — inline "Clear filters" button inside the
   * empty-state row. Reuses the quiet-button posture but with extra
   * top margin so it sits below the message. */
  .clear-filters {
    margin-top: var(--space-2);
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

  /* PR-213 / S215 — Kind column replaces the dropped Invoice id (UUID)
     column. Width is sized to the chip + the optional ExtNav source-
     NAV-invoice-number subtitle below it; the chip stays the leftmost
     row identifier (clickable to open the detail modal, same affordance
     the dropped UUID cell carried). Quiet aesthetic per ADR-0017 §1-2;
     the categorical signal is the chip's glyph + label per
     §"Adversarial review #4". */
  .col-row-kind {
    width: 18ch;
    vertical-align: top;
  }

  .row-kind-chip {
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
    cursor: pointer;
  }

  .row-kind-chip.kind-own {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .row-kind-chip.kind-ext-nav {
    color: var(--color-text-muted);
    border-color: var(--color-surface-divider);
  }

  .row-kind-glyph {
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    line-height: 1;
  }

  /* PR-213 / S215 — raw NAV invoice number for ExtNav rows. Muted
     subtitle under the Kind chip; gives the operator the readable
     identity of a NAV-mirror row without inflating into a separate
     column. Monospace + tabular so numeric NAV invoice numbers
     align vertically across rows. */
  .ext-nav-id {
    margin-top: var(--space-1);
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  /* PR-213 / S215 — em-dash placeholder for the State column on ExtNav
     rows. No NAV lifecycle is known from a `queryInvoiceDigest` row;
     surfacing the muted dash matches the Partner column's missing-
     buyer-name posture (operator's eye reads "no value" the same way). */
  .ext-nav-state-placeholder {
    color: var(--color-text-muted);
    font-family: var(--type-family-body);
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
   * Storno or Amended, both shorter). PR-95 / session-115 — the
   * NAV-status pictogram lands at the start of the cell (before
   * the state chip); widen to 26ch so the longest chip + pictogram
   * still fits without wrapping. */
  .col-state {
    width: 26ch;
  }

  /* PR-95 / session-115 — NAV-status pictogram. Quiet 1-em chip
   * carrying the 4-state vocab's glyph + per-state border colour
   * (kind-class). The four kinds map to existing ADR-0017 signal
   * tokens so the visual palette stays inside the design system.
   * The actionable arm (InFlight) is a <button> with cursor:pointer;
   * the three terminal arms are plain spans with cursor:help so the
   * tooltip-on-hover convention from the state pill is preserved. */
  .nav-pictogram {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 1.6em;
    height: 1.6em;
    margin-right: var(--space-1);
    padding: 0;
    border: 1px solid var(--color-surface-divider);
    border-radius: 2px;
    background: var(--color-surface-base);
    color: var(--color-text-secondary);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    line-height: 1;
    cursor: help;
    vertical-align: middle;
  }

  .nav-pictogram.pictogram-muted {
    color: var(--color-text-muted);
    border-color: var(--color-surface-divider);
  }
  /* PR-98 — `pictogram-submitted` is the new green-toned
   * positive-in-progress kind for the post-submit-pre-terminal
   * `Submitted` / `Recovered` lifecycle pair. Same colour token as
   * `pictogram-positive` (operator-positive signal: the submit
   * succeeded) but distinguished from the terminal-positive ✓
   * pictogram by the ⌛ glyph. The pre-PR-98 `pictogram-warning`
   * class is retired alongside the collapsed `InFlight` state. */
  .nav-pictogram.pictogram-submitted {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }
  .nav-pictogram.pictogram-negative {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .nav-pictogram.pictogram-positive {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  /* PR-95 — actionable variant is a <button>; reset native chrome +
   * give a pointer cursor so the affordance reads as "click me to
   * re-poll." Focus-visible outline mirrors the other quiet buttons
   * in the screen (sort headers, id-link). */
  button.nav-pictogram.actionable {
    cursor: pointer;
  }

  button.nav-pictogram.actionable:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  button.nav-pictogram.actionable:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 1px;
  }

  button.nav-pictogram.actionable:disabled {
    opacity: 0.7;
    cursor: progress;
  }

  button.nav-pictogram.busy {
    cursor: progress;
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

  /* S220 / PR-217 — clickable partner cell on ExtNav rows. The button
   * is borderless + background-less by default so the cell reads as
   * a plain label; only the underline-on-hover surfaces the
   * affordance. Distinct empty-state (em-dash) gets a dotted underline
   * so its "click me" affordance shows up without an obvious value to
   * underline. */
  .extnav-partner-link {
    background: transparent;
    border: none;
    padding: 0;
    margin: 0;
    cursor: pointer;
    color: inherit;
    font: inherit;
    text-align: left;
  }
  .extnav-partner-link:hover {
    text-decoration: underline;
    color: var(--color-link-hover, var(--color-text-primary));
  }
  .extnav-partner-link:focus-visible {
    outline: 2px solid var(--color-accent, #4a90e2);
    outline-offset: 2px;
    border-radius: 2px;
  }
  .extnav-partner-link-empty {
    text-decoration: underline dotted;
    text-underline-offset: 3px;
    color: var(--color-text-muted);
  }
  .extnav-partner-link-empty:hover {
    color: var(--color-link-hover, var(--color-text-primary));
    text-decoration: underline;
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
