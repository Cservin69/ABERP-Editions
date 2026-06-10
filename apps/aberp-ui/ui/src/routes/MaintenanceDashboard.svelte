<script lang="ts">
  // PR-79 / session 102 — maintenance area landing dashboard.
  //
  // The maintenance area (master data + settings) is one click away
  // from the operational sidebar, behind the topbar's ⚙ MAINTENANCE
  // gear (PR-78 / ADR-0041 §3). PR-78 left the gear navigating
  // straight to #/partners; PR-79 elevates the gear's destination to
  // a dedicated landing dashboard at #/maintenance so the operator
  // (a) sees a glanceable overview of every maintenance module + its
  // current state before drilling in, and (b) lands somewhere
  // deliberate, not at "the first module's first route by
  // accident".
  //
  // The dashboard is a tile grid: one tile per non-landing
  // maintenance route, grouped under sub-area headers (MASTER DATA,
  // SETTINGS). Each tile shows a bilingual label + description plus a
  // small live status — partner count, bank-account count, NAV
  // credential presence — fetched from EXISTING read-only backend
  // routes. No new backend in this PR (per the brief).
  //
  // Failure isolation: each tile's status fetch is independent;
  // erroring one does not block the others. Mirrors the PR-74 /
  // PR-75 loadError + retry pattern (banner pill on the offending
  // tile, every other tile keeps rendering). The honest "—" while
  // loading or on error is preferred over a fabricated number; the
  // operator should never see a misleading metric.

  import { onMount } from "svelte";

  import {
    getNavCredentialsStatus,
    getQuotingParameters,
    getSellerInfo,
    listAdapters,
    listComplexityRules,
    listEmailRelayQueue,
    listInventoryBalances,
    listLowStockProducts,
    listPartners,
    listQuotingMaterials,
    listProducts,
    listRestoredInvoices,
    listSellerBanks,
    listStockAdjustments,
    listToleranceMultipliers,
    type NavCredentialsStatusResponse,
    type SellerInfoResponse,
  } from "../lib/api";
  import {
    AREA_LABELS,
    MAINTENANCE_TILES,
    MODULES,
    type ErpModuleId,
    type MaintenanceTile,
  } from "../lib/erp-modules";
  import { navigateTo, routeHash } from "../lib/router";
  import { renderPushStatusSuffix } from "../lib/material-catalogue";

  // Per-tile load state — a discriminated union the chrome branches
  // on per render. We keep `value` typed loose-string here; each
  // statusKind's fetcher narrows below before composing the display
  // chip. The error path carries the verbatim error message so the
  // operator sees the actual cause when they hover the retry chip.
  type TileLoadState =
    | { kind: "loading" }
    | { kind: "loaded"; value: string }
    | { kind: "error"; message: string };

  // One per-tile load state, keyed by tile.route. Initialise every
  // entry to "loading" so the first paint never renders an empty
  // chip — `—` reads as honest absence, blank reads as broken UI.
  let tileStates: Record<string, TileLoadState> = $state(
    Object.fromEntries(
      MAINTENANCE_TILES.map((t) => [t.route, { kind: "loading" }]),
    ),
  );

  // Resolve the module display label for a tile's sub-area header
  // (MASTER DATA, SETTINGS). Falls through to the moduleId verbatim
  // if the registry has been edited inconsistently — better than
  // crashing the dashboard.
  function moduleLabel(id: ErpModuleId): string {
    const mod = MODULES.find((m) => m.id === id);
    return mod?.label_en ?? id;
  }

  function moduleGlyph(id: ErpModuleId): string {
    const mod = MODULES.find((m) => m.id === id);
    return mod?.glyph ?? "•";
  }

  // Group the flat tile list by moduleId, preserving registry order
  // both for modules and for tiles within each module. The dashboard
  // renders one `<section>` per non-empty group.
  type TileGroup = { moduleId: ErpModuleId; tiles: MaintenanceTile[] };
  let groupedTiles: TileGroup[] = $derived.by(() => {
    const seen = new Map<ErpModuleId, MaintenanceTile[]>();
    for (const t of MAINTENANCE_TILES) {
      const acc = seen.get(t.moduleId);
      if (acc === undefined) seen.set(t.moduleId, [t]);
      else acc.push(t);
    }
    // Order groups by the registry's module order, NOT by insertion
    // order in MAINTENANCE_TILES — the registry is the chrome's
    // canonical order source.
    const ordered: TileGroup[] = [];
    for (const m of MODULES) {
      const tiles = seen.get(m.id);
      if (tiles !== undefined && tiles.length > 0) {
        ordered.push({ moduleId: m.id, tiles });
      }
    }
    return ordered;
  });

  // PR-81 — fire-once trigger via `onMount`, NOT `$effect`. The
  // original `$effect` block created a self-cycle: `loadTileStatus`'
  // synchronous prefix does `tileStates = { ...tileStates, ... }`,
  // and the spread reads `tileStates` inside the effect's tracking
  // scope, so the write immediately re-invalidated the effect's own
  // dependency. Svelte 5 throws `effect_update_depth_exceeded` and
  // halts the global effect scheduler — which then dropped the
  // hashchange-driven `route` update in the App.svelte parent and
  // stranded the operator's `← OPERATIONAL` click on the
  // maintenance area. `onMount` runs outside the reactive graph,
  // so the spread/write pair is safe, and fire-once is the actual
  // intended semantics (the initial fetch trigger has no reactive
  // dependency — MAINTENANCE_TILES is a module-level const).
  onMount(() => {
    for (const tile of MAINTENANCE_TILES) {
      void loadTileStatus(tile);
    }
  });

  async function loadTileStatus(tile: MaintenanceTile): Promise<void> {
    tileStates = { ...tileStates, [tile.route]: { kind: "loading" } };
    try {
      const value = await fetchTileStatus(tile);
      tileStates = { ...tileStates, [tile.route]: { kind: "loaded", value } };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      tileStates = {
        ...tileStates,
        [tile.route]: { kind: "error", message },
      };
    }
  }

  // Closed-vocab dispatch on statusKind. A new status-kind is a
  // deliberate one-line widening here + an entry in `erp-modules.ts`'
  // `MaintenanceTileStatusKind` union. If the union widens without
  // this dispatch growing the matching arm, `tsc --strict` reports
  // an exhaustiveness failure on the `assertNever`-style fallthrough
  // below.
  async function fetchTileStatus(tile: MaintenanceTile): Promise<string> {
    switch (tile.statusKind) {
      case "PartnerCount": {
        const rows = await listPartners();
        const n = rows.length;
        return n === 1 ? "1 saved partner" : `${n} saved partners`;
      }
      case "ProductCount": {
        // S231 / PR-227 / ADR-0061 §6 — augment the product count
        // chip with a low-stock badge when ≥1 product is below min.
        // The dashboard reads the cached count from the virtual view
        // (one extra round-trip per dashboard load; tiny). On a
        // tenant with zero low-stock the chip looks identical to
        // pre-S231; the badge only surfaces when there is something
        // to act on, per [[hulye-biztos]] (do not show false-positive
        // alarms).
        const [rows, lowStock] = await Promise.all([
          listProducts(),
          listLowStockProducts().catch(() => [] as never[]),
        ]);
        const n = rows.length;
        const base = n === 1 ? "1 saved product" : `${n} saved products`;
        if (lowStock.length === 0) return base;
        const suffix =
          lowStock.length === 1
            ? "1 below min stock"
            : `${lowStock.length} below min stock`;
        return `${base} — ⚠ ${suffix}`;
      }
      case "BankAccountCount": {
        // Two independent reads — the seller-info legal_name + the
        // bank-account count. Combined here so the tile renders
        // both bits in one chip. Either failing falls through to
        // the catch above and marks the tile errored.
        const [info, banks] = await Promise.all([
          getSellerInfo(),
          listSellerBanks(),
        ]);
        return renderTenantStatus(info, banks.banks.length);
      }
      case "NavCredStatus": {
        const status = await getNavCredentialsStatus();
        return renderNavCredStatus(status);
      }
      case "RestoredInvoiceCount": {
        // S180 / PR-180 — count of rows in the local
        // `restored_invoice` mirror. "0 restored" reads as "DR not
        // yet exercised on this tenant" which is the operator's
        // expected default.
        const rows = await listRestoredInvoices();
        const n = rows.length;
        return n === 1 ? "1 restored invoice" : `${n} restored invoices`;
      }
      case "AdapterCount": {
        // S257 / PR-246 — count of registered MES adapters. "0
        // adapters" is the expected default for a tenant not running
        // the shop-floor strand.
        const rows = await listAdapters();
        const n = rows.length;
        return n === 1 ? "1 adapter" : `${n} adapters`;
      }
      case "MaterialCount": {
        // S266 / PR-255 — count of material grades in the auto-quoting
        // catalogue. Seeded with a handful of common grades on first
        // boot, so a fresh tenant shows that seed count, not 0.
        // S339 / PR-24 — append the live storefront push status so the
        // tile stops lying about catalogue delivery (the count read
        // green even while every push 403'd). Derived from the same
        // `push_status` the daemon records each cycle.
        const res = await listQuotingMaterials();
        const n = res.materials.length;
        const base = n === 1 ? "1 material" : `${n} materials`;
        const push = renderPushStatusSuffix(res.push_status, Date.now());
        return `${base} · ${push.text}`;
      }
      case "ComplexityRuleCount": {
        // S267 / PR-256 — count of complexity rules. Seeded empty on
        // a fresh tenant (operator-built over time) so 0 is expected.
        const res = await listComplexityRules();
        const n = res.rules.length;
        return n === 1 ? "1 rule" : `${n} rules`;
      }
      case "ToleranceMultiplierCount": {
        // S267 / PR-256 — count of tolerance multipliers. Seeded with
        // the five closed-vocab bands at boot.
        const res = await listToleranceMultipliers();
        const n = res.multipliers.length;
        return n === 1 ? "1 band" : `${n} bands`;
      }
      case "ParametersStatus": {
        // S267 / PR-256 — global parameters singleton. The seeded row
        // always exists; the chip surfaces the active `profit_margin_base`
        // so the operator sees at a glance whether defaults have been
        // tuned. `boot` updated_by_actor marks an untouched seed.
        const p = await getQuotingParameters();
        const pct = (p.profit_margin_base * 100).toFixed(0);
        const tuned = p.updated_by_actor === "boot" ? "default" : "tuned";
        return `margin ${pct}% · ${tuned}`;
      }
      case "StockAdjustmentCount": {
        // S267 / PR-256 — count of per-material × stock-status price
        // adjustments. Seeded empty so 0 is expected on a fresh tenant.
        const res = await listStockAdjustments();
        const n = res.adjustments.length;
        return n === 1 ? "1 adjustment" : `${n} adjustments`;
      }
      case "InventoryBalanceCount": {
        // S273 / PR-262 / ADR-0069 — count of `(tenant,
        // material_grade)` balance rows. Zero is expected on a fresh
        // tenant; the first DEAL auto-upserts a row at zero, then the
        // operator visits the page to set `on_hand_qty`.
        const res = await listInventoryBalances();
        const n = res.balances.length;
        return n === 1 ? "1 grade" : `${n} grades`;
      }
      case "EmailRelayQueueCount": {
        // S281 / PR-266 — count of `outbound_email_queue` rows across
        // all states. Zero is the most common state on a fresh tenant
        // (no storefront relay traffic). Surface a small breakdown
        // when there is anything in the queue so the operator sees
        // failed rows at a glance.
        const res = await listEmailRelayQueue();
        const n = res.rows.length;
        if (n === 0) return "0 rows";
        const failed = res.rows.filter((r) => r.state === "failed").length;
        if (failed > 0) return `${n} rows · ${failed} failed`;
        return n === 1 ? "1 row" : `${n} rows`;
      }
    }
  }

  function renderTenantStatus(
    info: SellerInfoResponse,
    bankCount: number,
  ): string {
    const name = info.legal_name.trim();
    const banks =
      bankCount === 1 ? "1 bank account" : `${bankCount} bank accounts`;
    if (name.length === 0) return banks;
    return `${name} · ${banks}`;
  }

  function renderNavCredStatus(status: NavCredentialsStatusResponse): string {
    // "Configured" only when all four slots are populated — a
    // partial configuration is honest about being incomplete rather
    // than fabricating a green state. NAV environment is currently
    // the test endpoint (the only one a single operator is wired
    // to); a future PR adds a prod/test toggle.
    const allPresent =
      status.login && status.password && status.sign_key && status.change_key;
    if (allPresent) return "Configured · Test endpoint";
    return "Not configured";
  }

  function tileStatusTone(state: TileLoadState): "loading" | "ok" | "error" {
    switch (state.kind) {
      case "loading":
        return "loading";
      case "loaded":
        return "ok";
      case "error":
        return "error";
    }
  }

  function statusDisplayLabel(state: TileLoadState): string {
    switch (state.kind) {
      case "loading":
        return "—";
      case "loaded":
        return state.value;
      case "error":
        return "load failed";
    }
  }

  function onTileClick(event: MouseEvent, tile: MaintenanceTile) {
    // The tile anchor's `href` already drives history; explicit
    // navigateTo() is belt-and-suspenders for jsdom (vitest)
    // environments that don't always fire hashchange on programmatic
    // hash assignment. Mirrors the sidebar route-row pattern.
    event.preventDefault();
    navigateTo(tile.route);
  }

  function onRetryClick(event: MouseEvent, tile: MaintenanceTile) {
    event.preventDefault();
    event.stopPropagation();
    void loadTileStatus(tile);
  }
</script>

<section class="dashboard" aria-labelledby="dashboard-title">
  <header class="dashboard__head">
    <h2 id="dashboard-title" class="dashboard__title">
      {AREA_LABELS.maintenance.en}
    </h2>
    <p class="dashboard__lede">
      Master data, identity, and credentials — the parts the operator
      touches rarely, but that the daily workflow depends on. Pick a
      tile to drill in.
    </p>
  </header>

  {#each groupedTiles as group (group.moduleId)}
    <section class="dashboard__group" aria-labelledby={`group-${group.moduleId}`}>
      <h3 class="dashboard__group-title" id={`group-${group.moduleId}`}>
        <span class="dashboard__group-glyph" aria-hidden="true">
          {moduleGlyph(group.moduleId)}
        </span>
        <span class="dashboard__group-label">
          {moduleLabel(group.moduleId)}
        </span>
      </h3>
      <ul class="dashboard__tiles">
        {#each group.tiles as tile (tile.route)}
          {@const state = tileStates[tile.route] ?? { kind: "loading" }}
          <li>
            <a
              class="tile"
              href={routeHash(tile.route)}
              data-route={tile.route}
              onclick={(e) => onTileClick(e, tile)}
            >
              <div class="tile__head">
                <span class="tile__label">{tile.label_en}</span>
              </div>
              <p class="tile__description">{tile.description_en}</p>
              <div class="tile__status" data-tone={tileStatusTone(state)}>
                <span class="tile__status-dot" aria-hidden="true"></span>
                <span class="tile__status-label">
                  {statusDisplayLabel(state)}
                </span>
                {#if state.kind === "error"}
                  <button
                    type="button"
                    class="tile__retry"
                    title={state.message}
                    onclick={(e) => onRetryClick(e, tile)}
                  >
                    Retry
                  </button>
                {/if}
              </div>
            </a>
          </li>
        {/each}
      </ul>
    </section>
  {/each}
</section>

<style>
  .dashboard {
    max-width: 1100px;
    margin: 0 auto;
  }

  .dashboard__head {
    margin-bottom: var(--space-5);
  }

  .dashboard__title {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-lg);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .dashboard__lede {
    margin: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
    max-width: 60ch;
  }

  .dashboard__group {
    margin-bottom: var(--space-5);
  }

  .dashboard__group-title {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0 0 var(--space-3) 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-muted);
    font-weight: 500;
  }

  .dashboard__group-glyph {
    display: inline-block;
    width: 14px;
    text-align: center;
    color: var(--color-text-muted);
  }

  .dashboard__group-label {
    line-height: 1;
  }

  .dashboard__tiles {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
    gap: var(--space-3);
  }

  .tile {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-4);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 6px;
    color: var(--color-text-primary);
    text-decoration: none;
    transition: border-color 0.12s ease, transform 0.12s ease;
  }

  .tile:hover {
    border-color: var(--color-text-muted);
    transform: translateY(-1px);
  }

  .tile:focus-visible {
    outline: 2px solid var(--color-signal-positive, var(--color-text-strong));
    outline-offset: 2px;
  }

  .tile__head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-2);
  }

  .tile__label {
    font-size: var(--type-size-md);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .tile__description {
    margin: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.4;
    flex: 1;
  }

  .tile__status {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    padding-top: var(--space-2);
    border-top: 1px dashed var(--color-surface-divider);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .tile__status-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--color-signal-muted);
  }

  .tile__status[data-tone="loading"] .tile__status-dot {
    background: var(--color-signal-muted);
    animation: aberp-tile-pulse 1.4s ease-in-out infinite;
  }

  .tile__status[data-tone="ok"] .tile__status-dot {
    background: var(--color-signal-positive);
  }

  .tile__status[data-tone="error"] .tile__status-dot {
    background: var(--color-signal-negative);
  }

  .tile__status[data-tone="error"] .tile__status-label {
    color: var(--color-signal-negative);
  }

  .tile__retry {
    margin-left: auto;
    padding: 2px var(--space-2);
    background: transparent;
    border: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    border-radius: 3px;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    cursor: pointer;
  }

  .tile__retry:hover {
    color: var(--color-text-strong);
    background: var(--color-surface-divider);
  }

  @keyframes aberp-tile-pulse {
    0%,
    100% {
      opacity: 0.4;
    }
    50% {
      opacity: 1;
    }
  }
</style>
