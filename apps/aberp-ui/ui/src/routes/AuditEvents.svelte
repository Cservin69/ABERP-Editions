<script lang="ts">
  // S424 / session-424 — cross-domain Audit-events screen. The general,
  // filterable, paginated view of the WHOLE tamper-evident ledger ("all
  // operator activity, any domain"), generalising the per-invoice
  // timeline. Ports the S411 PricingJobsList pattern (sortable headers,
  // filter chips, search box, empty-state Clear).
  //
  // ── Server vs client split (design §4.2 — pick ONE source per facet) ──
  //   * Domain CHIPS  → server `domains` (prefix) param; refetch on click
  //     so `total_matched` + pagination are correct.
  //   * Date from/to  → server `from`/`to` params; refetch on change.
  //   * Search box    → CLIENT-side instant refinement of the loaded rows
  //     (parseSearch mini-syntax) AND, on Enter/Refresh, its parsed
  //     operator/subject/text go to the server (operator/subject/q) for
  //     cross-page reach. The predicates are identical → applying both is
  //     idempotent ([[hulye-biztos]] — "what happened to quote X" in one
  //     click, whether X is on this page or an older one).
  //   * Sort          → CLIENT-side over the loaded rows (display only);
  //     server pagination is always seq-desc (newest first), so "Load
  //     more" appends older entries deterministically.
  //
  // Real-time: NO (Ervin 2026-06-15 #2). Manual Refresh + an opt-in
  // "auto-refresh every 60s" toggle, OFF by default.
  import { onMount } from "svelte";
  import {
    listAuditEvents,
    type AuditChainStatus,
    type AuditEventsQuery,
  } from "../lib/api";
  import {
    AUDIT_CHIPS,
    EMPTY_AUDIT_FILTER,
    domainOf,
    filterAuditEvents,
    isAuditFilterEmpty,
    kindLabel,
    parseSearch,
    prefixesForDomain,
    sortAuditEvents,
    type AuditDomain,
    type AuditEventRow,
    type AuditFilterSpec,
    type AuditSortKey,
    type SortDir,
  } from "../lib/audit-events-list";
  import { formatHungarianTimestamp } from "../lib/invoice-timeline";
  import AuditEventDetail from "../lib/AuditEventDetail.svelte";

  const PAGE_SIZE = 50;
  const AUTO_REFRESH_MS = 60_000;

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let rows = $state<AuditEventRow[]>([]);
  let chain = $state<AuditChainStatus | null>(null);
  let nextCursor = $state<number | null>(null);
  let totalMatched = $state(0);
  let loadingMore = $state(false);
  let now = $state(new Date());

  // Filter state. `domain` drives the server `domains` param; `search`
  // refines the loaded page client-side (and seeds the server q/op/subject
  // facets on refetch). TRANSIENT (not persisted) — an audit query is
  // investigative, not a long-lived view the operator returns to.
  let filter = $state<AuditFilterSpec>({ ...EMPTY_AUDIT_FILTER });
  let dateFrom = $state("");
  let dateTo = $state("");
  let sort = $state<{ key: AuditSortKey; dir: SortDir }>({ key: "seq", dir: "desc" });
  let selectedSeq = $state<number | null>(null);

  // Auto-refresh OFF by default (Ervin #2). The operator opts in.
  let autoRefresh = $state(false);

  // Client-side: filter the loaded rows by the live search box (domain is
  // already applied server-side → pass "all" here), then sort for display.
  let visibleRows = $derived(
    sortAuditEvents(
      filterAuditEvents(rows, { domain: "all", search: filter.search }),
      sort.key,
      sort.dir,
    ),
  );

  // Build the server query from the engaged facets.
  function buildQuery(cursor: number | null): AuditEventsQuery {
    const parsed = parseSearch(filter.search);
    const q: AuditEventsQuery = { limit: PAGE_SIZE };
    if (filter.domain !== "all") q.domains = prefixesForDomain(filter.domain).join(",");
    if (dateFrom) q.from = dateFrom;
    if (dateTo) q.to = dateTo;
    if (parsed.operator) q.operator = parsed.operator;
    if (parsed.subject) q.subject = parsed.subject;
    if (parsed.text) q.q = parsed.text;
    if (cursor !== null) q.afterSeq = cursor;
    return q;
  }

  onMount(() => {
    void refresh();
  });

  // Auto-refresh: re-run on the toggle changing. The interval calls
  // refresh() which reads the latest filter state at fire time.
  $effect(() => {
    if (!autoRefresh) return;
    const id = setInterval(() => void refresh(), AUTO_REFRESH_MS);
    return () => clearInterval(id);
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      const resp = await listAuditEvents(buildQuery(null));
      rows = resp.events;
      chain = resp.chain;
      nextCursor = resp.page.next_cursor;
      totalMatched = resp.page.total_matched;
      now = new Date();
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  async function loadMore(): Promise<void> {
    if (nextCursor === null || loadingMore) return;
    loadingMore = true;
    try {
      const resp = await listAuditEvents(buildQuery(nextCursor));
      rows = [...rows, ...resp.events];
      chain = resp.chain;
      nextCursor = resp.page.next_cursor;
      totalMatched = resp.page.total_matched;
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
    } finally {
      loadingMore = false;
    }
  }

  function onChipClick(domain: AuditDomain | "all"): void {
    filter = { ...filter, domain };
    void refresh();
  }

  function onSortClick(key: AuditSortKey): void {
    if (sort.key !== key) {
      sort = { key, dir: key === "seq" || key === "occurred_at" ? "desc" : "asc" };
      return;
    }
    if (sort.dir === "asc") {
      sort = { key, dir: "desc" };
      return;
    }
    sort = { key: "seq", dir: "desc" };
  }

  function sortIndicator(key: AuditSortKey): string {
    if (sort.key !== key) return "";
    return sort.dir === "asc" ? "▲" : "▼";
  }

  function ariaSort(key: AuditSortKey): "ascending" | "descending" | "none" {
    if (sort.key !== key) return "none";
    return sort.dir === "asc" ? "ascending" : "descending";
  }

  function clearFilters(): void {
    filter = { ...EMPTY_AUDIT_FILTER };
    dateFrom = "";
    dateTo = "";
    void refresh();
  }

  // A row's tamper status: its own hash_ok, AND not at/after a whole-
  // chain divergence (design §3.4 — rows from the break onward are ✗
  // regardless of their own per-entry hash).
  function rowChainOk(row: AuditEventRow): boolean {
    if (!row.hash_ok) return false;
    if (chain && !chain.verified && chain.first_divergence_seq !== null) {
      return row.seq < chain.first_divergence_seq;
    }
    return true;
  }

  function domainChipClass(kind: string): string {
    return `aud-kind aud-kind--${domainOf(kind)}`;
  }

  function shortId(id: string): string {
    if (id.length <= 16) return id;
    return `${id.slice(0, 10)}…${id.slice(-4)}`;
  }
</script>

<section class="audit">
  <header class="audit__hdr">
    <div>
      <h2>Tevékenységi napló / Activity log</h2>
      <p class="audit__sub">
        Minden naplózott operátori tevékenység és munkafolyamat /
        All logged operator activity & workflow
      </p>
    </div>
    <div class="audit__hdr-actions">
      <label class="audit__auto">
        <input
          type="checkbox"
          bind:checked={autoRefresh}
          data-testid="audit-auto-refresh"
        />
        <span>Auto-frissítés 60mp / Auto-refresh 60s</span>
      </label>
      <button
        type="button"
        class="btn btn--secondary"
        onclick={() => void refresh()}
        disabled={loadState === "loading"}
        data-testid="audit-refresh"
      >Frissítés / Refresh</button>
    </div>
  </header>

  <!-- Whole-chain tamper banner (design §3.4). Red when verify_chain
       failed; names the divergence seq + reason. -->
  {#if chain && !chain.verified}
    <div class="audit__chain-bad" data-testid="audit-chain-banner">
      <strong>⚠ A napló hash-lánca sérült / Audit chain integrity FAILED</strong>
      <p>
        Első eltérés a(z) <code>#{chain.first_divergence_seq}</code> sornál /
        First divergence at seq <code>#{chain.first_divergence_seq}</code>.
        {#if chain.reason}<br /><code>{chain.reason}</code>{/if}
      </p>
    </div>
  {:else if chain && chain.verified}
    <p class="audit__chain-ok" data-testid="audit-chain-ok">
      ✓ Hash-lánc ép a(z) #{chain.head_seq} sorig / Chain verified to seq #{chain.head_seq}
    </p>
  {/if}

  <!-- Controls: search + date + chips. -->
  <div class="audit__controls">
    <label class="audit__search">
      <span class="audit__lbl">Keresés / Search</span>
      <input
        type="search"
        value={filter.search}
        oninput={(e) =>
          (filter = { ...filter, search: (e.currentTarget as HTMLInputElement).value })}
        onkeydown={(e) => {
          if (e.key === "Enter") void refresh();
        }}
        placeholder="kind:… quote:… op:… vagy szabad szöveg / or free text"
        autocomplete="off"
        spellcheck="false"
        aria-label="Keresés a naplóban / Search the audit log"
        data-testid="audit-search"
      />
    </label>
    <label class="audit__date">
      <span class="audit__lbl">Tól / From</span>
      <input
        type="date"
        value={dateFrom}
        onchange={(e) => {
          dateFrom = (e.currentTarget as HTMLInputElement).value;
          void refresh();
        }}
        data-testid="audit-from"
      />
    </label>
    <label class="audit__date">
      <span class="audit__lbl">Ig / To</span>
      <input
        type="date"
        value={dateTo}
        onchange={(e) => {
          dateTo = (e.currentTarget as HTMLInputElement).value;
          void refresh();
        }}
        data-testid="audit-to"
      />
    </label>
  </div>
  <div class="audit__chips" role="group" aria-label="Terület szűrő / Domain filter">
    {#each AUDIT_CHIPS as chip (chip.domain)}
      <button
        type="button"
        class="audit__chip-btn"
        class:audit__chip-btn--active={filter.domain === chip.domain}
        aria-pressed={filter.domain === chip.domain}
        onclick={() => onChipClick(chip.domain)}
        data-testid={`audit-chip-${chip.domain}`}
      >{chip.label}</button>
    {/each}
  </div>

  {#if loadState === "loading" && rows.length === 0}
    <p class="audit__hint">Betöltés… / Loading…</p>
  {:else if loadState === "error"}
    <p class="audit__err" data-testid="audit-error">{errorMessage ?? "Hiba / Error"}</p>
  {:else}
    <p class="audit__count" data-testid="audit-count">
      {totalMatched} találat / matched · {rows.length} betöltve / loaded
    </p>
    <table class="audit__tbl" data-testid="audit-table">
      <thead>
        <tr>
          <th aria-sort={ariaSort("occurred_at")}>
            <button type="button" class="audit__sort" onclick={() => onSortClick("occurred_at")}>
              <span>Időpont / When</span>
              <span class="audit__sort-ind" aria-hidden="true">{sortIndicator("occurred_at")}</span>
            </button>
          </th>
          <th aria-sort={ariaSort("seq")}>
            <button type="button" class="audit__sort" onclick={() => onSortClick("seq")}>
              <span>Seq</span>
              <span class="audit__sort-ind" aria-hidden="true">{sortIndicator("seq")}</span>
            </button>
          </th>
          <th aria-sort={ariaSort("kind")}>
            <button type="button" class="audit__sort" onclick={() => onSortClick("kind")}>
              <span>Esemény / Event</span>
              <span class="audit__sort-ind" aria-hidden="true">{sortIndicator("kind")}</span>
            </button>
          </th>
          <th>Tárgy / Subject</th>
          <th aria-sort={ariaSort("actor")}>
            <button type="button" class="audit__sort" onclick={() => onSortClick("actor")}>
              <span>Operátor / Operator</span>
              <span class="audit__sort-ind" aria-hidden="true">{sortIndicator("actor")}</span>
            </button>
          </th>
          <th>Összegzés / Summary</th>
          <th>Lánc / Chain</th>
        </tr>
      </thead>
      <tbody>
        {#if visibleRows.length === 0}
          <tr class="audit__empty-row" data-testid="audit-empty">
            <td colspan="7">
              Nincs a szűrőnek megfelelő esemény. / No events match the current filter.
              {#if !isAuditFilterEmpty(filter) || dateFrom || dateTo}
                <button
                  type="button"
                  class="btn btn--secondary audit__clear"
                  onclick={clearFilters}
                  data-testid="audit-clear"
                >Szűrők törlése / Clear filters</button>
              {/if}
            </td>
          </tr>
        {/if}
        {#each visibleRows as row (row.id)}
          {@const ts = formatHungarianTimestamp(row.occurred_at, now)}
          <tr
            class="audit__row"
            tabindex="0"
            role="button"
            aria-label={`Részletek / Details #${row.seq}`}
            onclick={() => (selectedSeq = row.seq)}
            onkeydown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                selectedSeq = row.seq;
              }
            }}
            data-testid={`audit-row-${row.seq}`}
          >
            <td><time datetime={row.occurred_at} title={ts.absolute}>{ts.display}</time></td>
            <td><code>#{row.seq}</code></td>
            <td>
              <span class={domainChipClass(row.kind)} data-testid={`audit-kind-${row.seq}`}>
                {kindLabel(row.kind)}
              </span>
              <div class="audit__muted"><code>{row.kind}</code></div>
            </td>
            <td>
              {#if row.subject}<code title={row.subject}>{shortId(row.subject)}</code>{:else}—{/if}
            </td>
            <td>{row.actor}</td>
            <td class="audit__summary">{row.summary || "—"}</td>
            <td>
              {#if rowChainOk(row)}
                <span class="audit__ok" title="Hash ép / hash intact" data-testid={`audit-chain-${row.seq}`}>✓</span>
              {:else}
                <span class="audit__bad" title="Hash eltérés / hash mismatch" data-testid={`audit-chain-${row.seq}`}>✗</span>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>

    {#if nextCursor !== null}
      <div class="audit__more">
        <button
          type="button"
          class="btn btn--secondary"
          onclick={() => void loadMore()}
          disabled={loadingMore}
          data-testid="audit-load-more"
        >
          {loadingMore ? "Betöltés… / Loading…" : "Továbbiak / Load more"}
        </button>
      </div>
    {/if}
  {/if}

  <AuditEventDetail seq={selectedSeq} {rows} onClose={() => (selectedSeq = null)} />
</section>

<style>
  .audit {
    color: var(--color-text, #e5e7eb);
    padding: 16px;
  }
  .audit__hdr {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: 16px;
    margin-bottom: 12px;
    flex-wrap: wrap;
  }
  .audit__sub {
    font-size: 12px;
    color: var(--color-text-muted, #9ca3af);
    margin: 4px 0 0;
  }
  .audit__hdr-actions {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .audit__auto {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--color-text-muted, #9ca3af);
    cursor: pointer;
  }
  .audit__chain-bad {
    border: 1px solid var(--color-danger, #f87171);
    border-radius: var(--radius-md);
    background: var(--color-surface, #1f2937);
    color: var(--color-danger, #f87171);
    padding: 12px;
    margin-bottom: 12px;
  }
  .audit__chain-bad p {
    margin: 6px 0 0;
    font-size: 13px;
  }
  .audit__chain-ok {
    font-size: 12px;
    color: var(--color-ok, #34d399);
    margin: 0 0 12px;
  }
  .audit__controls {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-end;
    gap: 12px;
    margin-bottom: 8px;
  }
  .audit__search {
    display: flex;
    flex-direction: column;
    gap: 2px;
    flex: 1 1 320px;
    min-width: 240px;
  }
  .audit__date {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .audit__lbl {
    font-size: 11px;
    color: var(--color-text-muted, #9ca3af);
  }
  .audit__search input,
  .audit__date input {
    background: var(--color-surface, #1f2937);
    color: var(--color-text, #e5e7eb);
    border: 1px solid var(--color-border, #374151);
    border-radius: var(--radius-md);
    padding: 6px 10px;
    font-size: 13px;
  }
  .audit__search input:focus-visible,
  .audit__date input:focus-visible {
    outline: 2px solid var(--color-accent, #60a5fa);
    outline-offset: -1px;
  }
  .audit__chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 12px;
  }
  .audit__chip-btn {
    padding: 5px 12px;
    border-radius: var(--radius-pill);
    font-size: 12px;
    font-weight: 500;
    cursor: pointer;
    background: var(--color-surface, #1f2937);
    color: var(--color-text-muted, #9ca3af);
    border: 1px solid var(--color-border, #374151);
  }
  .audit__chip-btn:hover:not(.audit__chip-btn--active) {
    background: var(--color-surface-2, #243042);
    color: var(--color-text, #e5e7eb);
  }
  .audit__chip-btn--active {
    background: var(--color-accent, #2563eb);
    border-color: var(--color-accent, #2563eb);
    color: #f8fafc;
  }
  .audit__count {
    font-size: 12px;
    color: var(--color-text-muted, #9ca3af);
    margin: 0 0 8px;
  }
  .audit__hint,
  .audit__err {
    padding: 12px;
    border: 1px solid var(--color-border, #374151);
    border-radius: var(--radius-md);
    background: var(--color-surface, #1f2937);
  }
  .audit__err {
    border-color: var(--color-danger, #f87171);
    color: var(--color-danger, #f87171);
  }
  .audit__tbl {
    width: 100%;
    border-collapse: collapse;
  }
  .audit__tbl th,
  .audit__tbl td {
    text-align: left;
    padding: 8px 10px;
    border-bottom: 1px solid var(--color-border, #374151);
    font-size: 13px;
    vertical-align: top;
  }
  .audit__tbl th {
    font-weight: 600;
    color: var(--color-text-muted, #9ca3af);
  }
  .audit__sort {
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
  .audit__sort:hover {
    color: var(--color-text, #e5e7eb);
  }
  .audit__sort:focus-visible {
    outline: 2px solid var(--color-accent, #60a5fa);
    outline-offset: 2px;
  }
  .audit__sort-ind {
    font-size: 10px;
    color: var(--color-accent, #60a5fa);
    min-width: 8px;
  }
  .audit__row {
    cursor: pointer;
  }
  .audit__row:hover {
    background: var(--color-surface-2, #243042);
  }
  .audit__row:focus-visible {
    outline: 2px solid var(--color-accent, #60a5fa);
    outline-offset: -2px;
  }
  .audit__muted {
    color: var(--color-text-muted, #9ca3af);
    font-size: 11px;
  }
  .audit__summary {
    max-width: 320px;
  }
  .audit__empty-row td {
    color: var(--color-text-muted, #9ca3af);
    font-style: italic;
  }
  .audit__clear {
    margin-left: 10px;
    font-style: normal;
  }
  .audit__ok {
    color: var(--color-ok, #34d399);
  }
  .audit__bad {
    color: var(--color-danger, #f87171);
    font-weight: 700;
  }
  .audit__more {
    margin-top: 12px;
    text-align: center;
  }
  /* Domain-coloured kind chips. Glyph-free; the colour is a secondary
     cue (the wire kind under it is the categorical signal). */
  .aud-kind {
    display: inline-block;
    padding: 2px 8px;
    border-radius: var(--radius-pill);
    font-size: 12px;
    font-weight: 500;
    background: #374151;
    color: #f3f4f6;
  }
  .aud-kind--invoice {
    background: #1e3a8a;
    color: #bfdbfe;
  }
  .aud-kind--quote {
    background: #064e3b;
    color: #bbf7d0;
  }
  .aud-kind--email {
    background: #4c1d95;
    color: #ddd6fe;
  }
  .aud-kind--mes {
    background: #78350f;
    color: #fed7aa;
  }
  .aud-kind--inventory {
    background: #134e4a;
    color: #99f6e4;
  }
  .aud-kind--compliance {
    background: #7f1d1d;
    color: #fecaca;
  }
  .aud-kind--system {
    background: #374151;
    color: #d1d5db;
  }
</style>
