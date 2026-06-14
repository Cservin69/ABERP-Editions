<script lang="ts">
  // S211 / PR-210 — Quotes tab. Surfaces the daemon-staged
  // `quote_intake_log` rows for the operator. Read-only in S211: the
  // pickup action (operator-clicks → opens InvoiceCompose pre-populated
  // from the prepared draft) lands in S212. Until then, every visible
  // row carries the "Needs operator pickup" chip.
  //
  // Per S179's pattern, this is a third tab under Invoices alongside
  // Outgoing / Incoming — operator-clearer adjacency (quote-intake
  // produces draft invoices).
  //
  // Conservative-choice flags (S211b):
  //   - sort/filter persistence DELIBERATELY OUT OF SCOPE — the list
  //     is sorted by intake_at DESC server-side; the SPA does no
  //     additional re-ranking. S212 may add facets once the operator
  //     queue grows.
  //   - "Open invoice" link is disabled — there is NO billing.invoice
  //     row for these draft ids in S211 (the daemon stages a prepared
  //     draft JSON; the operator-clicked pickup creates the row).

  import { onMount } from "svelte";
  import {
    DealSagaError,
    dealQuote,
    listQuoteIntake,
    markQuoteIntakeIrrelevant,
    pickupQuoteAsDraft,
    refuseQuote,
    RefuseQuoteError,
    retryParseQuoteIntake,
    type QuoteIntakeRow,
  } from "../lib/api";
  import { formatInvoiceDate } from "../lib/format";
  import { validateRefuseReason } from "../lib/refuse-reason";
  import QuoteDealGate from "./QuoteDealGate.svelte";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState: LoadState = $state("idle");
  let errorMessage = $state<string | null>(null);
  let rows: QuoteIntakeRow[] = $state([]);
  // S255 / PR-244 — per-row pickup state. `null` keys reset to
  // "idle" on every refresh so a half-done click in the previous
  // list doesn't leave a spinner visible after a refresh.
  let pickupBusyQuoteId = $state<string | null>(null);
  let pickupError = $state<string | null>(null);
  // S256 / PR-245 — per-row recovery state for error (dead-letter) rows.
  let recoverBusyQuoteId = $state<string | null>(null);
  let recoverError = $state<string | null>(null);
  // S272 / PR-261 — per-row DEAL saga state. The error map is per-row
  // so a 409 on row A does NOT clear row B's prior error toast (each
  // QuoteDealGate component owns its own toast surface).
  let dealBusyQuoteId = $state<string | null>(null);
  let dealErrors = $state<Record<string, { code: string; message: string } | null>>({});
  // S403 — operator REFUSE-with-reason modal state. `refuseTarget` is the
  // quote_id being refused (null = modal closed); one page-level modal
  // serves every row. `refuseValidation` is the inline (client) gate;
  // `refuseError` is the server's rejection.
  let refuseTarget = $state<string | null>(null);
  let refuseReason = $state("");
  let refuseBusy = $state(false);
  let refuseValidation = $state<string | null>(null);
  let refuseError = $state<string | null>(null);

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      rows = await listQuoteIntake();
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

  function shortDraftId(id: string): string {
    // `drf_<26-char-ULID>` — show the prefix + last 4 so the operator
    // can tell two side-by-side drafts apart without copying the
    // full ULID.
    if (id.length <= 10) return id;
    return `${id.slice(0, 4)}…${id.slice(-4)}`;
  }

  // S255 / PR-244 — operator click handler. Calls the backend, then
  // navigates to the Invoices tab (where the new Draft row surfaces
  // under the [[aberp-invoice-draft-state]] chip). On idempotent
  // re-click of an already-picked-up quote, the backend returns the
  // same drf_id (200) and we still navigate.
  async function pickupQuote(row: QuoteIntakeRow): Promise<void> {
    pickupBusyQuoteId = row.quote_id;
    pickupError = null;
    try {
      const outcome = await pickupQuoteAsDraft(row.quote_id);
      // Mutate the in-memory row immediately so the operator sees the
      // "→ Draft" link without waiting for the refresh round-trip.
      // The next refresh() (manual or otherwise) will reconcile.
      const idx = rows.findIndex((r) => r.quote_id === row.quote_id);
      if (idx >= 0) {
        rows[idx] = { ...rows[idx], picked_up_drf_id: outcome.drf_id };
      }
      // Route the operator to the Invoices tab to see the new Draft.
      window.location.hash = "#/invoices";
    } catch (e) {
      pickupError =
        e instanceof Error ? e.message : String(e);
    } finally {
      pickupBusyQuoteId = null;
    }
  }

  // S256 / PR-245 (brief §A.4) — dead-letter recovery on an error row.
  // Retry re-parses the stored payload server-side; on success the row
  // flips to `staged` and we refresh so the pickup button appears.
  async function retryParse(row: QuoteIntakeRow): Promise<void> {
    recoverBusyQuoteId = row.quote_id;
    recoverError = null;
    try {
      await retryParseQuoteIntake(row.quote_id);
      await refresh();
    } catch (e) {
      recoverError = e instanceof Error ? e.message : String(e);
    } finally {
      recoverBusyQuoteId = null;
    }
  }

  // Dismiss a row (mark irrelevant). Drops it from the badge + queue.
  async function dismissRow(row: QuoteIntakeRow): Promise<void> {
    recoverBusyQuoteId = row.quote_id;
    recoverError = null;
    try {
      await markQuoteIntakeIrrelevant(row.quote_id);
      await refresh();
    } catch (e) {
      recoverError = e instanceof Error ? e.message : String(e);
    } finally {
      recoverBusyQuoteId = null;
    }
  }

  // S272 / PR-261 — DEAL saga submit. Per-row handler invoked by the
  // QuoteDealGate child once both the REFRESH (if `stock_alert=true`)
  // and DEAL-token gates are satisfied. On 200 the SPA mutates the row
  // in place so the DEAL UI disappears and the post-deal SO/WO chips
  // surface; on 409 the per-row error toast renders.
  async function submitDeal(
    row: QuoteIntakeRow,
    payload: { deal_token: string; refresh_ack: string | null },
  ): Promise<void> {
    dealBusyQuoteId = row.quote_id;
    dealErrors = { ...dealErrors, [row.quote_id]: null };
    try {
      const outcome = await dealQuote(row.quote_id, payload);
      const idx = rows.findIndex((r) => r.quote_id === row.quote_id);
      if (idx >= 0) {
        rows[idx] = {
          ...rows[idx],
          deal_issued_at: outcome.deal_issued_at,
          deal_sales_order_id: outcome.sales_order_id,
          deal_work_order_id: outcome.work_order_id,
          // S275 / PR-264 / F5 — flag the silent material-commit skip
          // so the post-DEAL chip warns the operator that no
          // reservation landed (storefront pushed NULL grade/qty).
          material_commit_skipped: outcome.material_commit === null,
        };
      }
    } catch (e) {
      const err = e instanceof DealSagaError
        ? { code: e.code, message: e.message }
        : { code: "unknown", message: e instanceof Error ? e.message : String(e) };
      dealErrors = { ...dealErrors, [row.quote_id]: err };
    } finally {
      dealBusyQuoteId = null;
    }
  }

  // S403 — open / close the refuse modal. Opening resets the prior
  // reason + error so a second row's modal never shows the first's text.
  function openRefuse(quoteId: string): void {
    refuseTarget = quoteId;
    refuseReason = "";
    refuseValidation = null;
    refuseError = null;
  }

  function closeRefuse(): void {
    refuseTarget = null;
    refuseReason = "";
    refuseValidation = null;
    refuseError = null;
  }

  // S403 — submit the refusal. Client-validates the reason first (the
  // server re-validates per [[trust-code-not-operator]]); on 200 the row
  // flips to `refused` server-side and drops out of the actionable queue
  // on refresh.
  async function submitRefuse(): Promise<void> {
    if (refuseTarget === null) return;
    const validation = validateRefuseReason(refuseReason);
    if (validation !== null) {
      refuseValidation = validation;
      return;
    }
    refuseBusy = true;
    refuseValidation = null;
    refuseError = null;
    try {
      await refuseQuote(refuseTarget, { reason: refuseReason.trim() });
      closeRefuse();
      await refresh();
    } catch (e) {
      refuseError =
        e instanceof RefuseQuoteError
          ? e.message
          : e instanceof Error
            ? e.message
            : String(e);
    } finally {
      refuseBusy = false;
    }
  }

  function openDraftFromPickup(): void {
    // The drafts-by-list view lives at #/invoices; the new Draft row
    // shows up with state=Draft. (A future PR can wire #/invoices/drafts/<id>
    // for a direct deep-link once the draft-detail SPA route lands.)
    window.location.hash = "#/invoices";
  }

  function writebackLabel(ts: string | null): {
    hu: string;
    en: string;
    tone: "pending" | "done";
  } {
    if (ts === null) {
      return {
        hu: "Visszajelzés függőben",
        en: "Writeback pending",
        tone: "pending",
      };
    }
    return {
      hu: "✓ Visszajelzés rendben",
      en: "Writeback complete",
      tone: "done",
    };
  }

  function fmt(ts: string): string {
    // Mirror IncomingInvoiceList: `formatInvoiceDate` handles the
    // common "yyyy-MM-dd" + "yyyy-MM-ddTHH:mm:ssZ" cases.
    return formatInvoiceDate(ts);
  }

  // S271 / PR-260 — EVE addendum 2 stale-stock guard. The recompute
  // happens server-side in `list_quote_intake_rows` (sticky downgrade
  // detection against `quoting_materials`); the SPA renders the loud
  // RED badge + the page-level banner. Closed-vocab discard: any row
  // whose `stock_alert` is neither `true` nor `false` (defensive
  // against API drift) is treated as `false`.
  let alertedCount = $derived(
    rows.filter((r) => r.stock_alert === true).length,
  );
  let alertedQuoteIds = $derived(
    rows
      .filter((r) => r.stock_alert === true)
      .map((r) => r.quote_id)
      .slice(0, 5),
  );

  function formatPriceEur(amount: number | null): string {
    if (amount === null || !Number.isFinite(amount)) return "—";
    // EUR with a NBSP between value and unit (per PR-249 PDF posture).
    return new Intl.NumberFormat("hu-HU", {
      minimumFractionDigits: 0,
      maximumFractionDigits: 2,
    }).format(amount) + " €";
  }
</script>

<section class="quotes-page" data-testid="quotes-list-section">
  <header class="quotes-page__head">
    <div>
      <h2 class="quotes-page__title">
        Ajánlatok / Quotes
        <span class="quotes-page__hint">
          ABERP-site-ról beérkezett, operátorra váró ajánlatok
        </span>
      </h2>
    </div>
    <div class="quotes-page__actions">
      <button
        type="button"
        class="quotes-page__refresh"
        disabled={loadState === "loading"}
        onclick={() => void refresh()}
        data-testid="quotes-refresh"
      >
        {loadState === "loading" ? "Frissítés…" : "Frissítés / Refresh"}
      </button>
    </div>
  </header>

  {#if pickupError !== null}
    <div class="quotes-page__error" role="alert" data-testid="quotes-pickup-error">
      <strong>Nem sikerült a piszkozatot létrehozni / Failed to create draft.</strong>
      <p class="quotes-page__error-detail">{pickupError}</p>
    </div>
  {/if}

  {#if recoverError !== null}
    <div class="quotes-page__error" role="alert" data-testid="quotes-recover-error">
      <strong>A művelet nem sikerült / Recovery action failed.</strong>
      <p class="quotes-page__error-detail">{recoverError}</p>
    </div>
  {/if}

  <!-- S271 / PR-260 — EVE addendum 2 stale-stock banner. Loud, top-of-
       page, RED. Surfaces the sticky `stock_alert` flag the backend
       recompute pass set when the material's stock_status downgraded
       since `stock_status_at_accept`. The DEAL gate (typed REFRESH
       token) lives in S272/PR-261; this PR ships the data half. -->
  {#if alertedCount > 0}
    <aside
      class="quotes-page__stock-alert"
      role="alert"
      data-testid="quotes-stock-alert-banner"
    >
      <strong class="quotes-page__stock-alert-head">
        ⚠️ {alertedCount} ajánlatnál megváltozott az anyag készletállapota a kiajánlás óta
        / {alertedCount} {alertedCount === 1 ? "quote has" : "quotes have"} a changed stock status since acceptance
      </strong>
      <p class="quotes-page__stock-alert-body">
        Írja be a REFRESH szót a DEAL előtt — a sornál lévő piros token-mezőbe.
        / Type REFRESH below to acknowledge the stock change before DEAL.
      </p>
      {#if alertedQuoteIds.length > 0}
        <p class="quotes-page__stock-alert-ids" data-testid="quotes-stock-alert-ids">
          {alertedQuoteIds.map((id) => shortQuoteId(id)).join(", ")}{alertedCount > alertedQuoteIds.length ? `, +${alertedCount - alertedQuoteIds.length}…` : ""}
        </p>
      {/if}
    </aside>
  {/if}

  {#if loadState === "loading" && rows.length === 0}
    <p class="quotes-page__muted">Betöltés… / Loading quotes…</p>
  {:else if loadState === "error"}
    <div class="quotes-page__error" role="alert">
      <strong>Nem sikerült lekérni az ajánlatokat / Failed to load quotes.</strong>
      <p class="quotes-page__error-detail">{errorMessage}</p>
    </div>
  {:else if rows.length === 0}
    <div class="quotes-page__empty" data-testid="quotes-empty">
      <p>
        Nincs még beérkezett ajánlat. Aktiváld az ajánlatfeladás daemont
        a Tenant beállítások &rarr; Ajánlatfeladás szekciónál (és indítsd
        újra az ABERP-et a változás érvényesítéséhez).
      </p>
      <p>
        No quotes staged yet. Enable the quote-intake daemon in
        Tenant Settings &rarr; Quote Intake (and restart ABERP for the
        change to take effect).
      </p>
    </div>
  {:else}
    <table class="quotes-table" data-testid="quotes-table">
      <thead>
        <tr>
          <th scope="col">Beérkezett / Received</th>
          <th scope="col">Stage-elve / Staged</th>
          <th scope="col">Ajánlat / Quote</th>
          <th scope="col">Vevő / Contact</th>
          <th scope="col">Anyag / Material</th>
          <th scope="col" class="quotes-table__num">Db / Qty</th>
          <!-- S271 / PR-260 — auto-quote total + EVE-addendum-2 stale-stock chip. -->
          <th scope="col" class="quotes-table__num">Ár (EUR) / Price</th>
          <th scope="col">Készlet / Stock</th>
          <th scope="col">Visszajelzés / Writeback</th>
          <th scope="col">Művelet / Action</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row (row.quote_id)}
          {@const wb = writebackLabel(row.status_writeback_at)}
          <tr
            data-testid="quotes-row"
            data-quote-id={row.quote_id}
            data-state={row.intake_state}
            data-stock-alert={row.stock_alert ? "true" : "false"}
            class:quotes-row--error={row.intake_state === "error"}
            class:quotes-row--stock-alert={row.stock_alert === true}
          >
            <td>{fmt(row.received_at)}</td>
            <td>{fmt(row.intake_at)}</td>
            <td>
              <code
                class="quotes-table__qid"
                title={row.quote_id}
                data-testid="quotes-row-id"
              >{shortQuoteId(row.quote_id)}</code>
            </td>
            <td>
              {#if row.contact_name}
                <div class="quotes-table__contact-name">{row.contact_name}</div>
              {/if}
              {#if row.contact_company}
                <div class="quotes-table__contact-company">{row.contact_company}</div>
              {/if}
              {#if row.contact_email}
                <div class="quotes-table__contact-email">{row.contact_email}</div>
              {:else if row.customer_email}
                <!-- S271: storefront-pushed typed email column. Falls
                     back when the raw payload didn't carry one. -->
                <div class="quotes-table__contact-email">{row.customer_email}</div>
              {/if}
              {#if !row.contact_name && !row.contact_email && !row.customer_email && !row.contact_company}
                <span class="quotes-table__muted">—</span>
              {/if}
            </td>
            <td>
              {#if row.material_grade}
                <!-- S271: typed closed-vocab grade takes precedence over
                     the raw-payload `material` blob. -->
                <div class="quotes-table__material-grade">{row.material_grade}</div>
              {:else if row.material}
                <div>{row.material}</div>
              {:else}
                <span class="quotes-table__muted">—</span>
              {/if}
              {#if row.notes}
                <div
                  class="quotes-table__notes"
                  title={row.notes}
                  data-testid="quotes-row-notes"
                >{row.notes}</div>
              {/if}
            </td>
            <td class="quotes-table__num">{row.quantity_canonical ?? row.quantity ?? "—"}</td>
            <td class="quotes-table__num">{formatPriceEur(row.total_price_eur)}</td>
            <td>
              {#if row.stock_alert === true}
                <!-- S271 / EVE addendum 2 — loud RED badge. Sticky:
                     persists across page reloads until operator REFRESH
                     (S272/PR-261). Title carries the snapshot vs current
                     so the operator can see WHY without opening detail. -->
                <span
                  class="quotes-chip quotes-chip--stock-alert"
                  data-testid="quotes-row-stock-alert"
                  title={row.stock_status_at_accept
                    ? `Acceptált: ${row.stock_status_at_accept}`
                    : "Stock changed since acceptance"}
                >⚠ Stock change</span>
              {:else if row.stock_status_at_accept}
                <span
                  class="quotes-chip quotes-chip--stock-ok"
                  data-testid="quotes-row-stock-ok"
                  title={`Accepted at ${row.stock_status_at_accept}`}
                >OK</span>
              {:else}
                <span class="quotes-table__muted">—</span>
              {/if}
            </td>
            <td>
              <span
                class="quotes-chip quotes-chip--{wb.tone}"
                data-testid="quotes-writeback-chip"
                title={wb.en}
              >{wb.hu}</span>
            </td>
            <td>
              {#if row.intake_state === "error"}
                <!-- S256 / PR-245 — dead-letter row: a malformed quote
                     the daemon staged instead of dropping. Show the
                     reason + recovery actions. -->
                <div
                  class="quotes-row__error-msg"
                  data-testid="quotes-row-error-msg"
                  title={row.intake_error ?? undefined}
                >
                  ⚠ {row.intake_error ?? "Hibás ajánlat / Malformed quote"}
                </div>
                <div class="quotes-row__error-actions">
                  <button
                    type="button"
                    class="quotes-row__pickup"
                    data-testid="quotes-row-retry-btn"
                    disabled={recoverBusyQuoteId === row.quote_id}
                    onclick={() => void retryParse(row)}
                    title="Re-parse the stored payload (S256)"
                  >
                    {recoverBusyQuoteId === row.quote_id
                      ? "…"
                      : "Újrapróbálás / Retry parse"}
                  </button>
                  <button
                    type="button"
                    class="quotes-row__dismiss"
                    data-testid="quotes-row-dismiss-btn"
                    disabled={recoverBusyQuoteId === row.quote_id}
                    onclick={() => void dismissRow(row)}
                    title="Mark this quote irrelevant (S256)"
                  >
                    Elvetés / Dismiss
                  </button>
                </div>
              {:else if row.deal_issued_at}
                <!-- S272 / PR-261 — post-DEAL state: SO + WO placeholder
                     chips. The SO module backfill will adopt the
                     `so_<ULID>` once it exists; until then this is a
                     static label, not a deep-link. -->
                <div
                  class="quotes-row__deal-done"
                  data-testid="quotes-row-deal-done"
                  title={`DEAL issued ${row.deal_issued_at}`}
                >
                  <span class="quotes-chip quotes-chip--done">✓ DEAL</span>
                  {#if row.material_commit_skipped}
                    <!-- S275 / PR-264 / F5 — the saga returned with
                         material_commit=null because the row carried
                         NULL `material_grade` and/or `quantity`. The
                         DEAL still booked SO/WO placeholders but NO
                         reservation landed in inventory_balances. The
                         operator needs to know — they may want to
                         hand-commit the material once the missing
                         field is filled in. -->
                    <span
                      class="quotes-chip quotes-chip--material-skip"
                      data-testid="quotes-row-material-skip"
                      title="material_commit was null — storefront pushed missing grade/qty"
                    >⚠ no material reservation</span>
                  {/if}
                  {#if row.deal_sales_order_id}
                    <code
                      class="quotes-row__deal-id"
                      data-testid="quotes-row-deal-so-id"
                    >SO {row.deal_sales_order_id.slice(-6)}</code>
                  {/if}
                  {#if row.deal_work_order_id}
                    <code
                      class="quotes-row__deal-id"
                      data-testid="quotes-row-deal-wo-id"
                    >WO {row.deal_work_order_id.slice(-6)}</code>
                  {/if}
                </div>
              {:else if row.picked_up_drf_id}
                <button
                  type="button"
                  class="quotes-row__draft-link"
                  data-testid="quotes-row-draft-link"
                  data-drf-id={row.picked_up_drf_id}
                  onclick={openDraftFromPickup}
                  title={`Draft: ${row.picked_up_drf_id}`}
                >
                  → Draft {shortDraftId(row.picked_up_drf_id)}
                </button>
                <!-- S272 — DEAL is a separate operator action from
                     pickup; surface it even when a draft was already
                     created (the saga commits SO/WO placeholders that
                     the future SO module will adopt). -->
                <QuoteDealGate
                  quoteId={row.quote_id}
                  stockAlert={row.stock_alert}
                  submitting={dealBusyQuoteId === row.quote_id}
                  error={dealErrors[row.quote_id] ?? null}
                  onSubmit={(payload) => void submitDeal(row, payload)}
                />
                <!-- S403 — DEAL's negative counterpart. Operator can't
                     fulfil (stock / capacity) → refuse with a reason; the
                     customer is e-mailed + the portal shows "Refused". -->
                <button
                  type="button"
                  class="quotes-row__refuse"
                  data-testid="quotes-row-refuse-btn"
                  onclick={() => openRefuse(row.quote_id)}
                  title="Decline this order with a reason — notifies the customer (S403)"
                >
                  Visszautasítás / Refuse
                </button>
              {:else}
                <button
                  type="button"
                  class="quotes-row__pickup"
                  data-testid="quotes-row-pickup-btn"
                  disabled={pickupBusyQuoteId === row.quote_id}
                  onclick={() => void pickupQuote(row)}
                  title="Create a draft invoice from this quote (S255)"
                >
                  {pickupBusyQuoteId === row.quote_id
                    ? "Létrehozás…"
                    : "Számla létrehozása / Create draft invoice"}
                </button>
                <!-- S272 / PR-261 — DEAL gate (EVE addenda 2 UI + 3).
                     REFRESH precondition + BIG/RED single-use token.
                     Shown alongside pickup so the operator can DEAL
                     without minting a draft first (the saga is its
                     own ADR-0067 unit). -->
                <QuoteDealGate
                  quoteId={row.quote_id}
                  stockAlert={row.stock_alert}
                  submitting={dealBusyQuoteId === row.quote_id}
                  error={dealErrors[row.quote_id] ?? null}
                  onSubmit={(payload) => void submitDeal(row, payload)}
                />
                <!-- S403 — DEAL's negative counterpart. Operator can't
                     fulfil (stock / capacity) → refuse with a reason; the
                     customer is e-mailed + the portal shows "Refused". -->
                <button
                  type="button"
                  class="quotes-row__refuse"
                  data-testid="quotes-row-refuse-btn"
                  onclick={() => openRefuse(row.quote_id)}
                  title="Decline this order with a reason — notifies the customer (S403)"
                >
                  Visszautasítás / Refuse
                </button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  <!-- S403 — operator REFUSE-with-reason modal. One page-level instance
       serves every row (keyed by `refuseTarget`). Required reason field
       (≥5 chars, client-validated + server-enforced); on submit the
       quote is refused, the customer e-mailed, and the storefront
       `rejected` status written back. [[spa-dark-theme-default]]. -->
  {#if refuseTarget !== null}
    <div class="refuse-modal__backdrop">
      <div
        class="refuse-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="refuse-modal-title"
        data-testid="refuse-modal"
      >
        <h3 class="refuse-modal__title" id="refuse-modal-title">
          Ajánlat visszautasítása / Refuse quote
        </h3>
        <p class="refuse-modal__hint">
          Az ügyfél e-mailben értesül az indokról. Számla NEM készül.
          / The customer is e-mailed the reason. No invoice is created.
        </p>
        <label class="refuse-modal__label" for="refuse-reason">
          Indok / Reason
        </label>
        <textarea
          id="refuse-reason"
          class="refuse-modal__textarea"
          data-testid="refuse-reason-input"
          rows="4"
          bind:value={refuseReason}
          disabled={refuseBusy}
          placeholder="Pl. anyaghiány, kapacitáshiány… / e.g. out of stock, no capacity…"
        ></textarea>
        {#if refuseValidation !== null}
          <p class="refuse-modal__validation" role="alert" data-testid="refuse-validation">
            {refuseValidation}
          </p>
        {/if}
        {#if refuseError !== null}
          <p class="refuse-modal__error" role="alert" data-testid="refuse-error">
            {refuseError}
          </p>
        {/if}
        <div class="refuse-modal__actions">
          <button
            type="button"
            class="refuse-modal__cancel"
            data-testid="refuse-cancel-btn"
            disabled={refuseBusy}
            onclick={closeRefuse}
          >
            Mégse / Cancel
          </button>
          <button
            type="button"
            class="refuse-modal__submit"
            data-testid="refuse-submit-btn"
            disabled={refuseBusy}
            onclick={() => void submitRefuse()}
          >
            {refuseBusy ? "Küldés… / Sending…" : "Visszautasítás / Refuse"}
          </button>
        </div>
      </div>
    </div>
  {/if}
</section>

<style>
  /* S226 / PR-222 — dark-theme colour polish. Same root cause as
   * StatisticsPage: this page (S211b/PR-210) referenced undefined token
   * names (--color-muted / --color-border / --color-surface[-alt] /
   * --text-* / --font-mono / --color-error*) and a handful of light-mode
   * hex literals, so it rendered washed-out on the dark theme. Every
   * colour now resolves to a tokens.css variable (ADR-0017); no new
   * tokens; no functional change. */
  .quotes-page {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) 0;
  }

  .quotes-page__head {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: var(--space-3);
    flex-wrap: wrap;
  }

  .quotes-page__title {
    font-size: var(--type-size-lg);
    font-weight: 600;
    margin: 0;
    color: var(--color-text-strong);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .quotes-page__hint {
    font-size: var(--type-size-sm);
    font-weight: 400;
    color: var(--color-text-muted);
  }

  .quotes-page__actions {
    display: flex;
    gap: var(--space-2);
  }

  .quotes-page__refresh {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .quotes-page__refresh:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quotes-page__refresh:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .quotes-page__muted {
    color: var(--color-text-muted);
    font-style: italic;
  }

  .quotes-page__error {
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-negative);
    border-radius: 3px;
    color: var(--color-text-primary);
  }

  .quotes-page__error strong {
    color: var(--color-signal-negative);
  }

  .quotes-page__error-detail {
    margin-top: var(--space-1);
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    color: var(--color-text-muted);
  }

  .quotes-page__empty {
    padding: var(--space-4);
    background: var(--color-surface-raised);
    border: 1px dashed var(--color-surface-divider);
    border-radius: 3px;
    color: var(--color-text-secondary);
  }

  .quotes-page__empty p + p {
    margin-top: var(--space-2);
  }

  .quotes-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
    background: var(--color-surface-sunken);
  }

  .quotes-table th,
  .quotes-table td {
    padding: var(--space-2) var(--space-3);
    text-align: left;
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
  }

  .quotes-table td {
    color: var(--color-text-primary);
  }

  .quotes-table th {
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .quotes-table tbody tr:hover {
    background: var(--color-surface-raised);
  }

  .quotes-table__num {
    text-align: right;
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-text-strong);
  }

  .quotes-table th.quotes-table__num {
    color: var(--color-text-muted);
  }

  .quotes-table__qid {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .quotes-table__contact-name {
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .quotes-table__contact-company {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .quotes-table__contact-email {
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
    color: var(--color-text-secondary);
  }

  .quotes-table__notes {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    max-width: 22rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .quotes-table__muted {
    color: var(--color-text-muted);
  }

  /* Status chips — categorical signal (ADR-0017 §"the 20%"): a coloured
   * label + matching hairline on a raised surface, no light fills. */
  .quotes-chip {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    font-weight: 500;
    white-space: nowrap;
  }

  .quotes-chip--pending {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  .quotes-chip--done {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  .quotes-chip--attention {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  /* S255 / PR-244 — pickup affordances. Match the dark-theme button
   * pattern from `.quotes-page__refresh` so the row-action stays
   * consistent with the page header's refresh button.
   * [[spa-dark-theme-default]] applies. */
  .quotes-row__pickup {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .quotes-row__pickup:hover:not(:disabled) {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  .quotes-row__pickup:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .quotes-row__draft-link {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-signal-positive);
    background: var(--color-surface-raised);
    color: var(--color-signal-positive);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    transition: color var(--motion-fade-in);
  }

  .quotes-row__draft-link:hover {
    color: var(--color-text-strong);
    border-color: var(--color-text-strong);
  }

  /* S256 / PR-245 — dead-letter (error-state) row affordances. A left
   * hairline in the warning signal colour marks the row; the message is
   * quiet (it's recoverable, not a hard failure). */
  .quotes-row--error td:first-child {
    border-left: 3px solid var(--color-signal-warning);
  }

  .quotes-row__error-msg {
    color: var(--color-signal-warning);
    font-size: var(--type-size-xs);
    max-width: 18rem;
    margin-bottom: var(--space-1);
  }

  .quotes-row__error-actions {
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
  }

  .quotes-row__dismiss {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .quotes-row__dismiss:hover:not(:disabled) {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .quotes-row__dismiss:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* S271 / PR-260 — EVE addendum 2 stale-stock surface.
   *
   * The data-side half of "the banner is NOT optional" per the brief:
   * a top-of-page red panel + a per-row red badge + a red left
   * hairline on every affected row. All `--color-signal-negative`
   * (the dark-theme RED token from ADR-0017) so the page-level
   * eyeball-test reads danger immediately.
   *
   * The typed REFRESH-token gate lives in S272/PR-261; here we ship the
   * visual surface so the operator knows the DEAL is gated before they
   * try to act on the row.
   */
  .quotes-page__stock-alert {
    padding: var(--space-3);
    border: 1px solid var(--color-signal-negative);
    border-left: 4px solid var(--color-signal-negative);
    background: var(--color-surface-sunken);
    border-radius: 3px;
    color: var(--color-text-strong);
  }

  .quotes-page__stock-alert-head {
    color: var(--color-signal-negative);
    display: block;
    font-size: var(--type-size-sm);
  }

  .quotes-page__stock-alert-body {
    margin-top: var(--space-1);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }

  .quotes-page__stock-alert-ids {
    margin-top: var(--space-1);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  /* Per-row RED hairline for stock_alert=true. Visually mirrors the
   * `quotes-row--error` warning hairline (S256) but uses
   * --color-signal-negative instead of --color-signal-warning so
   * stale-stock is unmistakable from a parser dead-letter. */
  .quotes-row--stock-alert td:first-child {
    border-left: 3px solid var(--color-signal-negative);
  }

  /* Per-row RED badge. Same chip shape as `.quotes-chip--pending`
   * but with --color-signal-negative as the foreground + border. */
  .quotes-chip--stock-alert {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
    font-weight: 600;
  }

  .quotes-chip--stock-ok {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  /* S275 / PR-264 / F5 — yellow chip on the post-DEAL state when the
   * saga's material branch was skipped silently (storefront pushed
   * NULL material_grade / quantity). Same `--color-signal-warning`
   * token the error-state row hairline uses, so the operator's eye
   * reads it as "needs attention but not critical." */
  .quotes-chip--material-skip {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  .quotes-table__material-grade {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-strong);
  }

  /* S272 / PR-261 — post-DEAL row affordance. SO + WO placeholder chips
   * displayed in the Action column once the saga has committed. The
   * `--color-signal-positive` chip + the mono SO/WO id labels match
   * the post-pickup `quotes-row__draft-link` shape, so the row visually
   * shifts from "pending action" → "complete + traceable". */
  .quotes-row__deal-done {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
    align-items: center;
  }

  .quotes-row__deal-id {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    background: var(--color-surface-raised);
    padding: 1px 6px;
    border-radius: 2px;
  }

  /* S403 — REFUSE affordance. Sits next to the DEAL gate as its negative
   * counterpart, so it reads in the RED signal colour (the decline
   * action) but stays quiet until hovered — DEAL is the primary path. */
  .quotes-row__refuse {
    margin-top: var(--space-1);
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .quotes-row__refuse:hover {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  /* S403 — refuse modal. Centred dialog over a dimmed backdrop; all
   * colours resolve to dark-theme tokens ([[spa-dark-theme-default]]). */
  .refuse-modal__backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
    padding: var(--space-3);
  }

  .refuse-modal {
    width: 100%;
    max-width: 32rem;
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-4);
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.4);
  }

  .refuse-modal__title {
    margin: 0 0 var(--space-1);
    font-size: var(--type-size-md);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .refuse-modal__hint {
    margin: 0 0 var(--space-3);
    font-size: var(--type-size-sm);
    color: var(--color-text-muted);
  }

  .refuse-modal__label {
    display: block;
    margin-bottom: var(--space-1);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-text-muted);
  }

  .refuse-modal__textarea {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    padding: var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    border-radius: 3px;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }

  .refuse-modal__textarea:focus {
    outline: none;
    border-color: var(--color-text-secondary);
  }

  .refuse-modal__validation,
  .refuse-modal__error {
    margin: var(--space-2) 0 0;
    font-size: var(--type-size-sm);
    color: var(--color-signal-negative);
  }

  .refuse-modal__actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }

  .refuse-modal__cancel {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }

  .refuse-modal__cancel:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .refuse-modal__submit {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    color: var(--color-signal-negative);
    border-radius: 3px;
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    font-weight: 600;
  }

  .refuse-modal__submit:hover:not(:disabled) {
    background: var(--color-signal-negative);
    color: var(--color-surface-sunken);
  }

  .refuse-modal__cancel:disabled,
  .refuse-modal__submit:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
