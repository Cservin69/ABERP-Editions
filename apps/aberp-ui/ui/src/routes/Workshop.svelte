<script lang="ts">
  // S235 / PR-231 — Workshop / Műhely operator dashboard.
  //
  // Wall-TV at-a-glance view of Stage 3 state: Work Orders by state,
  // low-stock product count, QA backlog, Dispatch panel, today's
  // invoice headline, recent audit-ledger activity, and MES adapter
  // env-config snapshot.
  //
  // One backend endpoint (`get_workshop_dashboard`) returns the whole
  // bundle in one shot; the SPA polls every ~10s. Per-tile refresh
  // re-fetches the bundle. No per-tile fetcher fan-out — cheaper than
  // six round-trips and the SPA stays simpler.
  //
  // Dark-theme tokens only per [[spa-dark-theme-default]]. Canonical
  // references: DispatchList.svelte (S234) + QaList.svelte (S233) +
  // StatisticsPage.svelte (S225 / PR-221).
  //
  // S238 / PR-232 — two changes ship together:
  //   1. Layout reorganised onto explicit `grid-template-areas`:
  //      Recent Activity becomes a full-height right rail; the
  //      Adapters + Low Stock + Today trio collapses to a bottom
  //      horizontal row of equal-width tiles. Same layout serves
  //      real + demo — only the data source swaps.
  //   2. Hidden demo-mode toggle: 5 clicks on the page H2 within 2s
  //      flips a localStorage flag; `api.ts.getWorkshopDashboard`
  //      then short-circuits to mock data. In demo mode, three
  //      kinetic effects bring the page alive — activity stream
  //      auto-scroll, spotlight rotation across tiles, scan-message
  //      ticker on the barcode-scanner adapter row. Real mode stays
  //      utilitarian (no animations).

  import { onMount, onDestroy } from "svelte";
  import {
    getWorkshopDashboard,
    type WorkshopDashboard,
    type WorkOrderStateCounts,
    type QaStateCounts,
    type WorkOrderRow,
  } from "../lib/api";
  import { navigateTo } from "../lib/router";
  import {
    adapterDotClass,
    adapterStatusLabel,
    fmtEventKind,
    fmtMinor,
    resolvePollInterval,
  } from "../lib/workshop-format";
  import {
    createTapDetector,
    isDemoMode,
    setDemoMode,
  } from "../lib/workshop-demo-mode";
  import {
    MOCK_SCAN_MESSAGES,
    MOCK_SPOTLIGHT_TILES,
  } from "../lib/workshop-mock-data";

  type LoadState = "idle" | "loading" | "ready" | "error";

  // Default poll cadence — 10s. Operator can override via the
  // `VITE_WORKSHOP_POLL_MS` env var read at build time. Bounded via
  // `resolvePollInterval` so a typo neither burns the backend nor
  // never refreshes.
  const POLL_INTERVAL_MS = resolvePollInterval(
    (import.meta as unknown as { env?: Record<string, string> }).env
      ?.VITE_WORKSHOP_POLL_MS,
    10_000,
  );

  // Debounce — Refresh button bursts get coalesced into one fetch.
  const REFRESH_DEBOUNCE_MS = 500;

  // Demo-mode kinetic intervals. Chosen for "feels alive without
  // distracting": scan ticker faster than spotlight, both fast
  // enough to register inside a 10-15s tile glance.
  const DEMO_SCAN_TICK_MS = 3_500;
  const DEMO_SPOTLIGHT_TICK_MS = 8_000;
  const DEMO_AUTO_SCROLL_TICK_MS = 6_000;

  let loadState: LoadState = $state("idle");
  let errorMessage = $state<string | null>(null);
  let bundle: WorkshopDashboard | null = $state(null);
  // Locale flip — match the bilingual chrome of QaList / DispatchList.
  let lang: "hu" | "en" = $state("hu");

  // ── Demo mode ────────────────────────────────────────────────────
  // Initial value pulled from localStorage so a mid-tour reload
  // preserves the operator's choice. Subsequent flips go via
  // `flipDemoMode()` which writes back to storage AND triggers an
  // immediate refresh so the tile values swap without waiting for
  // the next 10s poll tick.
  let demoMode = $state(isDemoMode());
  let scanTickIdx = $state(0);
  let spotlightIdx = $state(0);

  let pollTimer: ReturnType<typeof setInterval> | null = null;
  let scanTimer: ReturnType<typeof setInterval> | null = null;
  let spotlightTimer: ReturnType<typeof setInterval> | null = null;
  let autoScrollTimer: ReturnType<typeof setInterval> | null = null;
  let activityList: HTMLOListElement | null = $state(null);
  let lastRefreshAt = 0;
  let inFlight = $state(false);

  // 5-tap-within-2s gesture on the page H2. The handler is invisible
  // — no hover state, no tooltip, no "you've tapped N times" hint —
  // per [[trust-code-not-operator]] so a guest can't reverse-engineer
  // the gesture from chrome cues.
  const tapDetector = createTapDetector(() => {
    flipDemoMode();
  });

  function flipDemoMode(): void {
    const next = !demoMode;
    demoMode = next;
    setDemoMode(next);
    // Immediate refresh so the operator sees the value swap on the
    // tap, not 10s later.
    void refresh();
  }

  onMount(() => {
    void refresh();
    pollTimer = setInterval(() => {
      void refresh();
    }, POLL_INTERVAL_MS);
  });

  onDestroy(() => {
    if (pollTimer !== null) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
    stopDemoTimers();
  });

  function startDemoTimers(): void {
    stopDemoTimers();
    scanTimer = setInterval(() => {
      scanTickIdx = (scanTickIdx + 1) % MOCK_SCAN_MESSAGES.length;
    }, DEMO_SCAN_TICK_MS);
    spotlightTimer = setInterval(() => {
      spotlightIdx = (spotlightIdx + 1) % MOCK_SPOTLIGHT_TILES.length;
    }, DEMO_SPOTLIGHT_TICK_MS);
    autoScrollTimer = setInterval(() => {
      const el = activityList;
      if (el === null) return;
      // Smooth nudge so the tail of the list keeps moving into
      // view; once the bottom is reached, jump back to the top so
      // the cycle continues — operator-tour theater, not honest
      // pagination.
      const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 4;
      if (atBottom) {
        el.scrollTo({ top: 0, behavior: "smooth" });
      } else {
        el.scrollBy({ top: 48, behavior: "smooth" });
      }
    }, DEMO_AUTO_SCROLL_TICK_MS);
  }

  function stopDemoTimers(): void {
    if (scanTimer !== null) {
      clearInterval(scanTimer);
      scanTimer = null;
    }
    if (spotlightTimer !== null) {
      clearInterval(spotlightTimer);
      spotlightTimer = null;
    }
    if (autoScrollTimer !== null) {
      clearInterval(autoScrollTimer);
      autoScrollTimer = null;
    }
  }

  // React to demoMode flips: start / stop the theater timers
  // accordingly. `$effect` cleans up on demoMode→false automatically
  // because we explicitly call stopDemoTimers in the off branch.
  $effect(() => {
    if (demoMode) {
      startDemoTimers();
    } else {
      stopDemoTimers();
      scanTickIdx = 0;
      spotlightIdx = 0;
    }
  });

  async function refresh(): Promise<void> {
    // Refresh-storm protection per [[trust-code-not-operator]]: an
    // operator double-clicking the button does NOT issue two requests
    // back-to-back. The poll-driven tick also goes through this guard,
    // so a manual click immediately followed by the next tick coalesces.
    const now =
      typeof performance !== "undefined" && performance.now
        ? performance.now()
        : Date.now();
    if (inFlight) return;
    if (now - lastRefreshAt < REFRESH_DEBOUNCE_MS) return;
    lastRefreshAt = now;
    inFlight = true;

    if (loadState === "idle") loadState = "loading";
    try {
      const next = await getWorkshopDashboard();
      bundle = next;
      loadState = "ready";
      errorMessage = null;
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    } finally {
      inFlight = false;
    }
  }

  function toggleLang(): void {
    lang = lang === "hu" ? "en" : "hu";
  }

  // ── Click-through helpers ─────────────────────────────────────────
  //
  // For v1 we navigate to the module's list route; the operator selects
  // the desired state facet on the next screen. This keeps the
  // dashboard surgical per CLAUDE.md rule 3 — adding sessionStorage-
  // mediated cross-route filter init touches every list and is its
  // own PR if a future operator survey calls for it.

  function gotoWorkOrders(): void {
    navigateTo("work-orders");
  }
  function gotoProducts(): void {
    navigateTo("products");
  }
  function gotoQa(): void {
    navigateTo("qa");
  }
  function gotoDispatch(): void {
    navigateTo("dispatch");
  }
  function gotoStatistics(): void {
    navigateTo("statistics");
  }

  // ── Format helpers ────────────────────────────────────────────────

  function fmtRelativeTime(iso: string): string {
    if (iso === "" || iso === undefined) return "";
    const then = new Date(iso).getTime();
    if (Number.isNaN(then)) return iso;
    const diffSec = Math.round((then - Date.now()) / 1000);
    const abs = Math.abs(diffSec);
    const rtf = new Intl.RelativeTimeFormat(
      lang === "hu" ? "hu-HU" : "en-GB",
      { numeric: "auto" },
    );
    if (abs < 60) return rtf.format(diffSec, "second");
    if (abs < 3600) return rtf.format(Math.round(diffSec / 60), "minute");
    if (abs < 86_400) return rtf.format(Math.round(diffSec / 3600), "hour");
    return rtf.format(Math.round(diffSec / 86_400), "day");
  }

  // ── Localised labels ──────────────────────────────────────────────

  interface WoStateLabel {
    key: keyof WorkOrderStateCounts;
    hu: string;
    en: string;
  }
  const WO_STATE_LABELS: WoStateLabel[] = [
    { key: "created", hu: "Létrehozva", en: "Created" },
    { key: "released", hu: "Kiadva", en: "Released" },
    { key: "in_progress", hu: "Folyamatban", en: "In progress" },
    { key: "on_hold", hu: "Várakozik", en: "On hold" },
    { key: "completed", hu: "Kész", en: "Completed" },
    { key: "cancelled", hu: "Megszakítva", en: "Cancelled" },
  ];

  interface QaStateLabel {
    key: keyof QaStateCounts;
    hu: string;
    en: string;
  }
  // Pending + Reworking are the operator-actionable buckets per the
  // brief; the others are surfaced for completeness in a separate row
  // below so the tile reads as "what's blocking work" first.
  const QA_PRIMARY_LABELS: QaStateLabel[] = [
    { key: "pending", hu: "Függőben", en: "Pending" },
    { key: "reworking", hu: "Újramunkálás", en: "Reworking" },
  ];
  const QA_SECONDARY_LABELS: QaStateLabel[] = [
    { key: "passed", hu: "Sikeres", en: "Passed" },
    { key: "failed", hu: "Hibás", en: "Failed" },
    { key: "disposed", hu: "Selejt", en: "Disposed" },
  ];

  function fmtMinorWithLang(minor: number, currency: "HUF" | "EUR"): string {
    return fmtMinor(minor, currency, lang);
  }

  // Helper — `data-spotlight="true"` on the currently-highlighted
  // tile when demo mode is on. Off otherwise. The CSS rule for the
  // attribute does the actual border-glow animation.
  function spotlightFor(testid: string): "true" | "false" {
    if (!demoMode) return "false";
    return MOCK_SPOTLIGHT_TILES[spotlightIdx] === testid ? "true" : "false";
  }

  // ── S246 / PR-239 — wall-TV density helpers ────────────────────
  //
  // Maps the WO-state snake_case wire form onto the same HU/EN
  // label table the buckets-grid uses, so a row chip ("Folyamatban")
  // reads identically to its counterpart in the bucket above. The
  // mapping is the inverse of WO_STATE_LABELS' `key`-as-snake_case
  // assumption — when the brief widens the WO vocab, both surfaces
  // diverge at the same diff site.
  function woStateLabel(state: WorkOrderRow["state"]): string {
    const found = WO_STATE_LABELS.find((l) => l.key === state);
    if (found === undefined) return state;
    return lang === "hu" ? found.hu : found.en;
  }

  /** Format an HUF/EUR minor-unit total for the today-invoice row
   *  list. Mirrors `fmtMinorWithLang` but takes a `null`-tolerant
   *  input — the backend posture is `total_gross_minor: number | null`
   *  for rows without any lines yet. */
  function fmtRowMoney(
    minor: number | null,
    currency: "HUF" | "EUR",
  ): string {
    if (minor === null) return "—";
    return fmtMinor(minor, currency, lang);
  }
</script>

<section
  class="ws-page"
  aria-labelledby="ws-page-title"
  data-testid="workshop-page"
  data-demo-mode={demoMode ? "on" : "off"}
>
  <header class="ws-head">
    <div class="ws-head__titles">
      <!-- The H2 doubles as the hidden demo-mode handle. A normal
           click does nothing visible — the tap detector counts;
           five inside 2s flip the mode. Per the brief there is no
           hover state change, no tooltip, no progress hint — guest-
           invisible by design. The H2 stays a heading for AT (we
           attach onclick directly rather than wrapping in a button
           so the heading semantics are preserved). The gesture is
           mouse/touch-only by design — keyboard activation would
           expose the affordance via focus ring + tabindex, so we
           suppress the matching a11y rule rather than satisfy it. -->
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
      <h2
        id="ws-page-title"
        class="ws-head__title-tap"
        onclick={() => tapDetector.tap()}
      >
        {lang === "hu" ? "Műhely" : "Workshop"}
      </h2>
      <p class="ws-head__sub">
        {lang === "hu"
          ? "Gyártás állapota, élőben"
          : "Production at a glance"}
      </p>
    </div>
    <div class="ws-head__actions">
      <button
        type="button"
        class="ws-head__btn"
        onclick={toggleLang}
        aria-label={lang === "hu" ? "Switch to English" : "Magyar nyelvre"}
      >
        {lang === "hu" ? "EN" : "HU"}
      </button>
      <button
        type="button"
        class="ws-head__btn"
        onclick={() => void refresh()}
        disabled={inFlight}
        data-testid="workshop-refresh-all"
      >
        {lang === "hu" ? "Frissítés" : "Refresh"}
      </button>
      {#if bundle !== null}
        <span class="ws-head__stamp" title={bundle.snapshot_at_iso8601}>
          {fmtRelativeTime(bundle.snapshot_at_iso8601)}
        </span>
      {/if}
    </div>
    {#if demoMode}
      <!-- Operator-only indicator — tour-invisible by virtue of size
           + opacity. Marks the corner so the operator knows demo is
           on without giving the gesture away to a guest. -->
      <span
        class="ws-head__demo-dot"
        aria-label="demo mode"
        data-testid="workshop-demo-indicator"
      ></span>
    {/if}
  </header>

  {#if loadState === "loading" && bundle === null}
    <p class="ws-status">
      {lang === "hu" ? "Betöltés…" : "Loading…"}
    </p>
  {:else if loadState === "error" && bundle === null}
    <p class="ws-status ws-status--error" role="alert">
      {lang === "hu" ? "Hiba" : "Error"}: {errorMessage ?? "?"}
      <button
        type="button"
        class="ws-head__btn"
        onclick={() => void refresh()}>{lang === "hu" ? "Újra" : "Retry"}</button
      >
    </p>
  {/if}

  {#if bundle !== null}
    {@const b = bundle}
    <div class="ws-grid" role="region" aria-label="dashboard tiles">
      <!-- Work Orders by state — top of the left side, spans the
           three left columns. -->
      <article
        class="ws-tile ws-tile--wo"
        aria-labelledby="tile-wo-title"
        data-testid="tile-work-orders"
        data-spotlight={spotlightFor("tile-work-orders")}
      >
        <header class="ws-tile__head">
          <h3 id="tile-wo-title">
            {lang === "hu" ? "Munkalapok" : "Work orders"}
          </h3>
          <button
            type="button"
            class="ws-tile__link"
            onclick={gotoWorkOrders}>{lang === "hu" ? "Lista →" : "Open →"}</button
          >
        </header>
        <ul class="ws-grid-inner">
          {#each WO_STATE_LABELS as label}
            <li class="ws-stat">
              <button
                type="button"
                class="ws-stat__btn"
                onclick={gotoWorkOrders}
                data-testid={`wo-stat-${label.key}`}
              >
                <span class="ws-stat__value">{b.work_orders[label.key]}</span>
                <span class="ws-stat__label">
                  {lang === "hu" ? label.hu : label.en}
                </span>
              </button>
            </li>
          {/each}
        </ul>
        <!-- S246 / PR-239 — recent WO rows underneath the bucket grid.
             5-row cap per the density brief; empty-state hidden so the
             tile shape stays stable when a fresh tenant has no WOs. -->
        {#if (b.work_order_rows ?? []).length > 0}
          <ul class="ws-rows" data-testid="wo-row-list">
            {#each b.work_order_rows ?? [] as row (row.wo_id)}
              <li class="ws-row" data-testid={`wo-row-${row.wo_id}`}>
                <span class="ws-row__primary">{row.wo_number}</span>
                <span class="ws-row__secondary">{row.product_name}</span>
                <span class={`ws-row__chip ws-row__chip--${row.state}`}>
                  {woStateLabel(row.state)}
                </span>
                <time
                  class="ws-row__time"
                  datetime={row.touched_at_iso8601}
                  title={row.touched_at_iso8601}
                >
                  {fmtRelativeTime(row.touched_at_iso8601)}
                </time>
              </li>
            {/each}
          </ul>
        {/if}
      </article>

      <!-- QA backlog — middle-row left, narrower. -->
      <article
        class="ws-tile ws-tile--qa"
        aria-labelledby="tile-qa-title"
        data-testid="tile-qa"
        data-spotlight={spotlightFor("tile-qa")}
      >
        <header class="ws-tile__head">
          <h3 id="tile-qa-title">
            {lang === "hu" ? "Minőségellenőrzés" : "QA queue"}
          </h3>
          <button
            type="button"
            class="ws-tile__link"
            onclick={gotoQa}>{lang === "hu" ? "Sor →" : "Open →"}</button
          >
        </header>
        <ul class="ws-qa-primary">
          {#each QA_PRIMARY_LABELS as label}
            <li class="ws-stat">
              <button
                type="button"
                class="ws-stat__btn"
                onclick={gotoQa}
                data-testid={`qa-stat-${label.key}`}
              >
                <span
                  class={`ws-stat__value ${b.qa[label.key] > 0 ? "ws-stat__value--warn" : ""}`}
                >
                  {b.qa[label.key]}
                </span>
                <span class="ws-stat__label">
                  {lang === "hu" ? label.hu : label.en}
                </span>
              </button>
            </li>
          {/each}
        </ul>
        <p class="ws-qa-secondary">
          {#each QA_SECONDARY_LABELS as label, i}
            <span class="ws-qa-pair">
              {lang === "hu" ? label.hu : label.en}: {b.qa[label.key]}
            </span>
            {#if i < QA_SECONDARY_LABELS.length - 1}<span
                class="ws-qa-sep"
                aria-hidden="true">·</span
              >{/if}
          {/each}
        </p>
        <!-- S246 / PR-239 — Pending QA list (up to 7 oldest-first). -->
        {#if (b.pending_qa_rows ?? []).length > 0}
          <ul class="ws-rows" data-testid="qa-row-list">
            {#each b.pending_qa_rows ?? [] as row (row.qa_id)}
              <li class="ws-row" data-testid={`qa-row-${row.qa_id}`}>
                <span class="ws-row__primary">{row.wo_number}</span>
                <span class="ws-row__secondary">{row.op_name}</span>
                <time
                  class="ws-row__time"
                  datetime={row.created_at_iso8601}
                  title={row.created_at_iso8601}
                >
                  {fmtRelativeTime(row.created_at_iso8601)}
                </time>
              </li>
            {/each}
          </ul>
        {/if}
      </article>

      <!-- Dispatch board — middle-row right, takes the remaining
           two columns. -->
      <article
        class="ws-tile ws-tile--dispatch"
        aria-labelledby="tile-dispatch-title"
        data-testid="tile-dispatch"
        data-spotlight={spotlightFor("tile-dispatch")}
      >
        <header class="ws-tile__head">
          <h3 id="tile-dispatch-title">
            {lang === "hu" ? "Kiszállítás" : "Dispatch"}
          </h3>
          <button
            type="button"
            class="ws-tile__link"
            onclick={gotoDispatch}>{lang === "hu" ? "Tábla →" : "Open →"}</button
          >
        </header>
        <ul class="ws-grid-inner ws-grid-inner--narrow">
          <li class="ws-stat">
            <button
              type="button"
              class="ws-stat__btn"
              onclick={gotoDispatch}
              data-testid="dispatch-eligible"
            >
              <span class="ws-stat__value">
                {b.dispatch.eligible_work_orders}
              </span>
              <span class="ws-stat__label">
                {lang === "hu" ? "Indítható WO" : "Eligible WOs"}
              </span>
            </button>
          </li>
          <li class="ws-stat">
            <button
              type="button"
              class="ws-stat__btn"
              onclick={gotoDispatch}
              data-testid="dispatch-drafted"
            >
              <span class="ws-stat__value">
                {b.dispatch.by_state.drafted}
              </span>
              <span class="ws-stat__label">
                {lang === "hu" ? "Tervezet" : "Drafted"}
              </span>
            </button>
          </li>
          <li class="ws-stat">
            <button
              type="button"
              class="ws-stat__btn"
              onclick={gotoDispatch}
              data-testid="dispatch-shipped-today"
            >
              <span class="ws-stat__value">{b.dispatch.shipped_today}</span>
              <span class="ws-stat__label">
                {lang === "hu" ? "Ma kiszállítva" : "Shipped today"}
              </span>
            </button>
          </li>
        </ul>
        <!-- S246 / PR-239 — Dispatch density rows.
             Two sub-lists below the counters: Eligible WOs (5 cap) and
             Pending Drafted dispatches (2 cap). Each sub-list has its
             own header so the operator distinguishes "ready to dispatch"
             from "drafted, awaiting ship." -->
        {#if (b.eligible_dispatch_rows ?? []).length > 0}
          <div class="ws-rows-group">
            <h4 class="ws-rows-group__title">
              {lang === "hu" ? "Indítható" : "Eligible"}
            </h4>
            <ul class="ws-rows" data-testid="eligible-dispatch-row-list">
              {#each b.eligible_dispatch_rows ?? [] as row (row.wo_id)}
                <li
                  class="ws-row"
                  data-testid={`eligible-dispatch-row-${row.wo_id}`}
                >
                  <span class="ws-row__primary">{row.wo_number}</span>
                  <span class="ws-row__secondary">{row.product_name}</span>
                  <span class="ws-row__qty">
                    {row.qty_target}
                  </span>
                  <time
                    class="ws-row__time"
                    datetime={row.completed_at_iso8601}
                    title={row.completed_at_iso8601}
                  >
                    {fmtRelativeTime(row.completed_at_iso8601)}
                  </time>
                </li>
              {/each}
            </ul>
          </div>
        {/if}
        {#if (b.pending_dispatch_rows ?? []).length > 0}
          <div class="ws-rows-group">
            <h4 class="ws-rows-group__title">
              {lang === "hu" ? "Tervezet" : "Pending"}
            </h4>
            <ul class="ws-rows" data-testid="pending-dispatch-row-list">
              {#each b.pending_dispatch_rows ?? [] as row (row.dsp_id)}
                <li
                  class="ws-row"
                  data-testid={`pending-dispatch-row-${row.dsp_id}`}
                >
                  <span class="ws-row__primary">{row.wo_number}</span>
                  <span class="ws-row__secondary">{row.partner_name}</span>
                  <time
                    class="ws-row__time"
                    datetime={row.created_at_iso8601}
                    title={row.created_at_iso8601}
                  >
                    {fmtRelativeTime(row.created_at_iso8601)}
                  </time>
                </li>
              {/each}
            </ul>
          </div>
        {/if}
      </article>

      <!-- Bottom-row trio — Adapters, Low Stock, Today. Three equal-
           width tiles per [[aberp-workshop-demo-mode]] layout brief. -->
      <article
        class="ws-tile ws-tile--adapters"
        aria-labelledby="tile-adapters-title"
        data-testid="tile-adapters"
        data-spotlight={spotlightFor("tile-adapters")}
      >
        <header class="ws-tile__head">
          <h3 id="tile-adapters-title">
            {lang === "hu" ? "Adapterek" : "Adapters"}
          </h3>
        </header>
        {#if b.adapters.length === 0}
          <p class="ws-empty">
            {lang === "hu"
              ? "Nincs adapter regisztrálva"
              : "No adapters registered"}
          </p>
        {:else}
          <ul class="ws-adapter-list">
            {#each b.adapters as adapter}
              <li
                class="ws-adapter"
                data-testid={`adapter-${adapter.name}`}
              >
                <span
                  class={`ws-dot ${adapterDotClass(adapter.status)}`}
                  aria-hidden="true"
                ></span>
                <div class="ws-adapter__body">
                  <span class="ws-adapter__name">{adapter.name}</span>
                  <span class="ws-adapter__meta">
                    {adapter.kind}{adapter.port > 0
                      ? ` · ${adapter.host}:${adapter.port}`
                      : ""}
                  </span>
                  {#if demoMode && adapter.kind === "barcode-scanner"}
                    <!-- Demo-mode polish: a rolling "last scan"
                         line on the barcode-scanner adapter row,
                         cycling MOCK_SCAN_MESSAGES every ~3.5s. -->
                    <span
                      class="ws-adapter__scan"
                      data-testid="adapter-scan-message"
                    >
                      {lang === "hu" ? "Beolvasva" : "Scanned"}:
                      {MOCK_SCAN_MESSAGES[scanTickIdx]}
                    </span>
                  {/if}
                </div>
                <span class={`ws-pill ws-pill--${adapter.status}`}>
                  {adapterStatusLabel(adapter.status, lang)}
                </span>
              </li>
            {/each}
          </ul>
        {/if}
      </article>

      <!-- Low stock — bottom row middle. -->
      <article
        class="ws-tile ws-tile--lowstock"
        aria-labelledby="tile-low-stock-title"
        data-testid="tile-low-stock"
        data-spotlight={spotlightFor("tile-low-stock")}
      >
        <header class="ws-tile__head">
          <h3 id="tile-low-stock-title">
            {lang === "hu" ? "Készlethiány" : "Low stock"}
          </h3>
          <button
            type="button"
            class="ws-tile__link"
            onclick={gotoProducts}>{lang === "hu" ? "Termékek →" : "Open →"}</button
          >
        </header>
        <button
          type="button"
          class={`ws-bignum ${b.low_stock_products.count > 0 ? "ws-bignum--warn" : ""}`}
          onclick={gotoProducts}
          data-testid="low-stock-count"
        >
          <span class="ws-bignum__value">{b.low_stock_products.count}</span>
          <span class="ws-bignum__label">
            {lang === "hu"
              ? "minimum alatti termék"
              : "products below minimum"}
          </span>
        </button>
        <!-- S246 / PR-239 — list of below-min product rows (up to 10).
             Each row reads "name — qty/min — bin" so the operator scans
             the actual items without leaving the dashboard. -->
        {#if (b.low_stock_rows ?? []).length > 0}
          <ul class="ws-rows" data-testid="low-stock-row-list">
            {#each b.low_stock_rows ?? [] as row (row.product_id)}
              <li class="ws-row" data-testid={`low-stock-row-${row.product_id}`}>
                <span class="ws-row__primary">{row.name}</span>
                <span class="ws-row__qty ws-row__qty--warn">
                  {row.stock_qty}
                  <span class="ws-row__qty-sep" aria-hidden="true">/</span>
                  {row.min_stock}
                </span>
                <span class="ws-row__secondary">
                  {row.bin_location === ""
                    ? lang === "hu"
                      ? "nincs hely"
                      : "no bin"
                    : row.bin_location}
                </span>
              </li>
            {/each}
          </ul>
        {/if}
      </article>

      <!-- Today snapshot — bottom row right. -->
      <article
        class="ws-tile ws-tile--today"
        aria-labelledby="tile-today-title"
        data-testid="tile-today"
        data-spotlight={spotlightFor("tile-today")}
      >
        <header class="ws-tile__head">
          <h3 id="tile-today-title">
            {lang === "hu" ? "Ma" : "Today"}
            <span class="ws-tile__hint">({b.today.date})</span>
          </h3>
          <button
            type="button"
            class="ws-tile__link"
            onclick={gotoStatistics}
            >{lang === "hu" ? "Statisztika →" : "Open →"}</button
          >
        </header>
        <ul class="ws-grid-inner ws-grid-inner--narrow">
          <li class="ws-stat">
            <span class="ws-stat__value">
              {b.today.issued_count_huf + b.today.issued_count_eur}
            </span>
            <span class="ws-stat__label">
              {lang === "hu" ? "Kiállított számla" : "Issued invoices"}
            </span>
          </li>
          <li class="ws-stat">
            <span class="ws-stat__value ws-stat__value--money">
              {fmtMinorWithLang(b.today.gross_revenue_huf_minor, "HUF")}
            </span>
            <span class="ws-stat__label">
              {lang === "hu" ? "Bruttó HUF" : "Gross HUF"}
            </span>
          </li>
          {#if b.today.gross_revenue_eur_minor !== 0 || b.today.issued_count_eur > 0}
            <li class="ws-stat">
              <span class="ws-stat__value ws-stat__value--money">
                {fmtMinorWithLang(b.today.gross_revenue_eur_minor, "EUR")}
              </span>
              <span class="ws-stat__label">
                {lang === "hu" ? "Bruttó EUR" : "Gross EUR"}
              </span>
            </li>
          {/if}
        </ul>
        <!-- S246 / PR-239 — issued-today invoice rows (up to 5).
             Backend caps at 5 + emits `today_invoice_total`; the footer
             renders "+N more" when the cap truncates so the operator
             knows there's overflow rather than reading "only 5 today." -->
        {#if (b.today_invoice_rows ?? []).length > 0}
          <ul class="ws-rows" data-testid="today-invoice-row-list">
            {#each b.today_invoice_rows ?? [] as row (row.invoice_id)}
              <li class="ws-row" data-testid={`today-invoice-row-${row.invoice_id}`}>
                <span class="ws-row__primary">
                  {row.buyer_name === ""
                    ? lang === "hu"
                      ? "ismeretlen vevő"
                      : "unknown buyer"
                    : row.buyer_name}
                </span>
                <span class="ws-row__qty">
                  {fmtRowMoney(row.total_gross_minor, row.currency)}
                </span>
                <span class="ws-row__secondary">
                  {row.sequence_number}/{row.fiscal_year}
                </span>
              </li>
            {/each}
          </ul>
          {#if (b.today_invoice_total ?? 0) > (b.today_invoice_rows ?? []).length}
            <p class="ws-rows-more" data-testid="today-invoice-overflow">
              {lang === "hu"
                ? `+${(b.today_invoice_total ?? 0) - (b.today_invoice_rows ?? []).length} további`
                : `+${(b.today_invoice_total ?? 0) - (b.today_invoice_rows ?? []).length} more`}
            </p>
          {/if}
        {/if}
      </article>

      <!-- Recent activity — full-height right rail. -->
      <article
        class="ws-tile ws-tile--recent"
        aria-labelledby="tile-recent-title"
        data-testid="tile-recent-activity"
        data-spotlight={spotlightFor("tile-recent-activity")}
      >
        <header class="ws-tile__head">
          <h3 id="tile-recent-title">
            {lang === "hu" ? "Friss események" : "Recent activity"}
          </h3>
        </header>
        {#if b.recent_activity.length === 0}
          <p class="ws-empty">
            {lang === "hu" ? "Még nincs esemény" : "Nothing yet"}
          </p>
        {:else}
          <ol
            class={`ws-activity ${demoMode ? "ws-activity--demo" : ""}`}
            bind:this={activityList}
          >
            {#each b.recent_activity as entry (entry.id)}
              <li class="ws-activity__row">
                <span class="ws-activity__kind">{fmtEventKind(entry.kind)}</span>
                <time
                  class="ws-activity__time"
                  datetime={entry.at_iso8601}
                  title={entry.at_iso8601}
                >
                  {fmtRelativeTime(entry.at_iso8601)}
                </time>
              </li>
            {/each}
          </ol>
        {/if}
      </article>
    </div>
  {/if}
</section>

<style>
  /* S235 / PR-231 — Workshop dashboard dark-theme styles. Tokens only;
     no hardcoded hex. Canonical references per
     [[spa-dark-theme-default]]: DispatchList.svelte (S234) + QaList.svelte
     (S233) + StatisticsPage.svelte (S225).

     S238 / PR-232 — layout rewritten onto `grid-template-areas` so
     Recent Activity is a tall right rail and the
     Adapters / Low Stock / Today trio forms an equal-width bottom
     row. The CSS-grid template makes the layout intent explicit
     so a future refactor can't accidentally hide a tile by tweaking
     a column count. Demo-mode kinetic styles append at the bottom. */

  .ws-page {
    padding: var(--space-4);
    color: var(--color-text-primary);
    background: var(--color-surface-base);
    min-height: 100vh;
  }

  .ws-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-4);
    position: relative;
  }

  .ws-head__titles h2 {
    margin: 0;
    font-size: var(--type-size-xxl);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  /* H2 doubles as the demo-mode tap target. We keep the heading
     looking exactly like a heading (no underline, no hand cursor)
     so the gesture stays invisible to guests; the operator's
     muscle memory does the work. */
  .ws-head__title-tap {
    cursor: default;
    user-select: none;
  }

  .ws-head__sub {
    margin: var(--space-1) 0 0 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }

  .ws-head__actions {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }

  .ws-head__btn {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border-radius: 4px;
    cursor: pointer;
  }

  .ws-head__btn:hover:not(:disabled) {
    border-color: var(--color-text-muted);
  }

  .ws-head__btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .ws-head__stamp {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
  }

  /* Operator-only demo-mode dot. 6px square, 30% opacity, pinned to
     the absolute corner of the header. Visible if you know to
     look for it; invisible to a guest scanning the tile values. */
  .ws-head__demo-dot {
    position: absolute;
    top: 0;
    right: 0;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--color-signal-positive);
    opacity: 0.3;
    pointer-events: none;
  }

  .ws-status {
    padding: var(--space-3);
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }

  .ws-status--error {
    color: var(--color-signal-negative);
  }

  /* Explicit grid: four columns (3 left + 1 right rail), three
     rows. Named areas make the layout intent obvious AND the
     responsive collapse below trivial.

     Row sizing: WO tile is content-tall (six WO state buckets in a
     3×2 inner grid); QA + Dispatch row is content-tall; bottom
     trio is content-tall. The right rail spans all three rows,
     so its tile height equals the sum of the left rows' heights. */
  .ws-grid {
    display: grid;
    grid-template-columns: 1fr 1fr 1fr minmax(280px, 1fr);
    grid-template-rows: auto auto auto;
    grid-template-areas:
      "wo wo wo recent"
      "qa dispatch dispatch recent"
      "adapters lowstock today recent";
    gap: var(--space-3);
  }

  .ws-tile {
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    border-radius: 6px;
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-width: 0;
  }

  .ws-tile--wo {
    grid-area: wo;
  }
  .ws-tile--qa {
    grid-area: qa;
  }
  .ws-tile--dispatch {
    grid-area: dispatch;
  }
  .ws-tile--adapters {
    grid-area: adapters;
  }
  .ws-tile--lowstock {
    grid-area: lowstock;
  }
  .ws-tile--today {
    grid-area: today;
  }
  .ws-tile--recent {
    grid-area: recent;
    /* Tall right-rail flex column so the activity list inside
       can claim the leftover vertical space. */
    min-height: 0;
  }

  .ws-tile__head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-2);
  }

  .ws-tile__head h3 {
    margin: 0;
    font-size: var(--type-size-md);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .ws-tile__hint {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
    margin-left: var(--space-1);
  }

  .ws-tile__link {
    background: transparent;
    color: var(--color-text-secondary);
    border: 0;
    padding: 0;
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    cursor: pointer;
  }

  .ws-tile__link:hover {
    color: var(--color-text-strong);
  }

  .ws-grid-inner {
    list-style: none;
    padding: 0;
    margin: 0;
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: var(--space-2);
  }

  .ws-grid-inner--narrow {
    grid-template-columns: repeat(3, 1fr);
  }

  .ws-qa-primary {
    list-style: none;
    padding: 0;
    margin: 0;
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: var(--space-2);
  }

  .ws-qa-secondary {
    margin: var(--space-2) 0 0 0;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }

  .ws-qa-pair {
    margin-right: var(--space-1);
  }

  .ws-qa-sep {
    color: var(--color-text-muted);
    margin: 0 var(--space-1);
  }

  .ws-stat {
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-2);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    min-width: 0;
  }

  /* Stat buttons keep text alignment + colours of the static stat
     when they are not "clickable" — operator should not see a button
     chrome on a passive number. */
  .ws-stat__btn {
    background: transparent;
    border: 0;
    padding: 0;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .ws-stat__btn:hover .ws-stat__value {
    color: var(--color-text-strong);
  }

  .ws-stat__value {
    font-size: var(--type-size-xl);
    font-weight: 500;
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }

  .ws-stat__value--warn {
    color: var(--color-signal-warning);
  }

  .ws-stat__value--money {
    font-size: var(--type-size-lg);
  }

  .ws-stat__label {
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
  }

  .ws-bignum {
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-3);
    color: inherit;
    text-align: left;
    cursor: pointer;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font: inherit;
  }

  .ws-bignum:hover {
    border-color: var(--color-text-muted);
  }

  .ws-bignum__value {
    font-size: var(--type-size-xxl);
    font-weight: 500;
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }

  .ws-bignum--warn .ws-bignum__value {
    color: var(--color-signal-warning);
  }

  .ws-bignum__label {
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }

  .ws-empty {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
    font-style: italic;
  }

  /* ── S246 / PR-239 — density rows ──────────────────────────────
     Row styling mirrors the existing `.ws-adapter` (surface-raised
     pill on the sunken tile background) so the operator's eye
     reads "list-of-items below the aggregate" without a visual
     break. Dark tokens only per [[spa-dark-theme-default]]. */

  .ws-rows {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .ws-row {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-1) var(--space-2);
    font-size: var(--type-size-xs);
    min-width: 0;
  }

  /* Primary cell — the row's identifying label (WO number,
     product name, buyer name). Mono so a long-running TV view
     reads the digits at a constant width. */
  .ws-row__primary {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* Secondary cell — context label (product on a WO row, op name
     on a QA row, bin location on a low-stock row). Subdued so the
     primary cell stays the focal point. */
  .ws-row__secondary {
    color: var(--color-text-secondary);
    flex: 0 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* Right-aligned timestamp cell. Mono for the same constant-width
     reason. Muted because absolute time is rarely the actionable
     signal on a wall TV. */
  .ws-row__time {
    color: var(--color-text-muted);
    font-family: var(--type-family-mono);
    margin-left: auto;
    flex: 0 0 auto;
    white-space: nowrap;
  }

  /* Quantity / money cell. Same mono treatment as the primary
     cell; warning variant for the low-stock "qty/min" pair so the
     undersupply reads at a glance. */
  .ws-row__qty {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    flex: 0 0 auto;
    margin-left: auto;
    white-space: nowrap;
  }

  .ws-row__qty--warn {
    color: var(--color-signal-warning);
  }

  .ws-row__qty-sep {
    color: var(--color-text-muted);
    margin: 0 2px;
  }

  /* WO-state chip on a row. Color inherits from the existing
     signal vocab so a row chip matches the bucket-grid color
     intent without re-stating tokens. `created` + `released`
     read as neutral; `in_progress` is positive; `on_hold` warns;
     terminal states fade. */
  .ws-row__chip {
    font-size: var(--type-size-xs);
    padding: 0 var(--space-2);
    border-radius: 999px;
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-secondary);
    flex: 0 0 auto;
  }

  .ws-row__chip--in_progress {
    color: var(--color-signal-positive);
  }

  .ws-row__chip--on_hold {
    color: var(--color-signal-warning);
  }

  .ws-row__chip--completed,
  .ws-row__chip--cancelled {
    color: var(--color-text-muted);
  }

  /* Dispatch tile holds two sub-lists (Eligible + Pending) and
     wants each titled. Title styling is minimal — secondary text
     + xs size — so the visual weight stays on the rows below. */
  .ws-rows-group {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .ws-rows-group__title {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-weight: 400;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  /* "+N more" footer on the Today tile when the row list truncates.
     Muted, italic, no border so it reads as overflow signal, not
     as another row. */
  .ws-rows-more {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-style: italic;
    text-align: right;
  }

  .ws-adapter-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .ws-adapter {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-2);
  }

  .ws-adapter__body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .ws-adapter__name {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .ws-adapter__meta {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }

  /* Demo-mode "last scan" line. Subtle italic so it reads as a
     transient note rather than configured chrome. */
  .ws-adapter__scan {
    color: var(--color-signal-positive);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    font-style: italic;
    margin-top: 2px;
  }

  .ws-dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    flex: 0 0 auto;
    background: var(--color-text-muted);
  }

  .ws-dot--positive {
    background: var(--color-signal-positive);
  }

  .ws-dot--warning {
    background: var(--color-signal-warning);
  }

  .ws-dot--negative {
    background: var(--color-signal-negative);
  }

  .ws-dot--muted {
    background: var(--color-text-muted);
  }

  .ws-pill {
    font-size: var(--type-size-xs);
    padding: 2px var(--space-2);
    border-radius: 999px;
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
  }

  /* S240 / PR-234 — chip variants for the live-registry adapter vocab.
     Colours come from the existing signal tokens so dark-theme
     contrast is inherited per [[spa-dark-theme-default]]. */
  .ws-pill--healthy {
    color: var(--color-signal-positive);
  }

  .ws-pill--degraded,
  .ws-pill--starting {
    color: var(--color-signal-warning);
  }

  .ws-pill--unhealthy {
    color: var(--color-signal-negative);
  }

  .ws-pill--stopped {
    color: var(--color-text-muted);
  }

  /* Activity list — tall right rail. `min-height: 0` on the
     wrapper tile lets the flex column shrink so this `flex: 1`
     panel claims the leftover vertical space. */
  .ws-activity {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }

  /* Demo-mode adds a smooth scroll behaviour so the auto-scroll
     timer's `scrollBy` calls glide rather than jump. Real mode
     keeps the default (operator scrubs the bar manually). */
  .ws-activity--demo {
    scroll-behavior: smooth;
  }

  .ws-activity__row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-bottom: 1px solid var(--color-surface-divider);
    font-size: var(--type-size-xs);
  }

  .ws-activity__kind {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }

  .ws-activity__time {
    color: var(--color-text-muted);
  }

  /* Demo-mode spotlight — one tile at a time gets a soft border-
     glow that fades in. Animation is the OUTER cycle (8s) divided
     into a ~2s fade in / 4s hold / 2s fade out via opacity on a
     pseudo-element rather than on the tile border itself — that
     way the glow has no layout effect and the real border colour
     stays untouched. */
  .ws-tile {
    position: relative;
  }

  .ws-tile[data-spotlight="true"]::after {
    content: "";
    position: absolute;
    inset: -2px;
    border-radius: 8px;
    pointer-events: none;
    border: 1px solid var(--color-signal-positive);
    box-shadow: 0 0 12px var(--color-signal-positive);
    opacity: 0;
    animation: ws-spotlight-pulse 8s ease-in-out;
  }

  @keyframes ws-spotlight-pulse {
    0% {
      opacity: 0;
    }
    25% {
      opacity: 0.45;
    }
    75% {
      opacity: 0.45;
    }
    100% {
      opacity: 0;
    }
  }

  /* ── Responsive collapse ───────────────────────────────────────
     Wide TV (≥1600px): the default layout as drawn — 4 cols,
       Recent Activity tall right rail.
     Laptop (1280-1600px): same shape; the columns just narrow.
     Narrow laptop / tablet (<1280px): drop the right rail under
       everything else; left side reflows to 2 cols on the trio.
     Phone (<720px): single-column stack of every tile in source
       order. */
  @media (max-width: 1280px) {
    .ws-grid {
      grid-template-columns: 1fr 1fr 1fr;
      grid-template-areas:
        "wo wo wo"
        "qa dispatch dispatch"
        "adapters lowstock today"
        "recent recent recent";
    }
  }

  @media (max-width: 960px) {
    .ws-grid {
      grid-template-columns: 1fr 1fr;
      grid-template-areas:
        "wo wo"
        "qa dispatch"
        "adapters lowstock"
        "today today"
        "recent recent";
    }
    .ws-grid-inner {
      grid-template-columns: repeat(2, 1fr);
    }
  }

  @media (max-width: 720px) {
    .ws-grid {
      grid-template-columns: 1fr;
      grid-template-areas:
        "wo"
        "qa"
        "dispatch"
        "adapters"
        "lowstock"
        "today"
        "recent";
    }
  }
</style>
