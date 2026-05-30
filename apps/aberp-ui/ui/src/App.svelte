<script lang="ts">
  // Root component — owns the boot-lifecycle gate AND the health
  // probe AND the invoice-list mount.
  //
  // PR-45a / session-61 — pre-PR-45a this component mounted
  // InvoiceList unconditionally and the operator stared at a blank
  // pane while the backend cold-booted (5-10s on a fresh launch).
  // The component now polls `getBootStatus()` and renders one of
  // three view-modes via `bootViewMode`:
  //
  //   - `loading`  — animated indicator + the latest backend log
  //                  line (forwarded from `aberp serve`'s stderr).
  //                  Polls every 300ms until the lifecycle leaves
  //                  the `starting` state.
  //   - `error`    — boot-failed pane with the verbatim error
  //                  message + a Retry button + the bullet list of
  //                  common causes (`FAILURE_HINTS`). The Retry
  //                  button calls `retryBoot()`, which re-spawns
  //                  the backend; the polling loop picks up the
  //                  new lifecycle transparently.
  //   - `ready`    — mount the existing InvoiceList screen. Stops
  //                  the boot poller and starts the existing 10s
  //                  /health probe.
  //
  // ADR-0017 puts the first dense-table screen at the centre;
  // everything around it is chrome. The header carries one signal
  // token (the backend liveness dot) and one text label (the ABERP
  // wordmark). No search, no settings, no nav — those land in
  // subsequent PRs as their underlying routes ship.

  import { onDestroy, onMount } from "svelte";
  import {
    getBootStatus,
    health,
    retryBoot,
    type BootStatusResponse,
    type HealthResponse,
  } from "./lib/api";
  import {
    bootErrorMessage,
    bootViewMode,
    FAILURE_HINTS,
    latestLogLine,
    type BootViewMode,
  } from "./lib/boot-status";
  import {
    AREA_LABELS,
    areaForRoute,
    defaultRouteForArea,
    modulesInArea,
    moduleForRoute,
    type ErpArea,
  } from "./lib/erp-modules";
  import {
    currentRoute,
    navigateTo,
    routeHash,
    subscribeRoute,
    type AppRoute,
  } from "./lib/router";
  import FirstProdLaunchModal from "./routes/FirstProdLaunchModal.svelte";
  import { shouldShowFirstProdLaunchModal } from "./lib/first-prod-launch";
  import InvoiceList from "./routes/InvoiceList.svelte";
  import IncomingInvoiceList from "./routes/IncomingInvoiceList.svelte";
  import IssueInvoice from "./routes/IssueInvoice.svelte";
  import MaintenanceDashboard from "./routes/MaintenanceDashboard.svelte";
  import NavCredentialsSettings from "./routes/NavCredentialsSettings.svelte";
  import PartnersList from "./routes/PartnersList.svelte";
  import ProductsList from "./routes/ProductsList.svelte";
  import SellerConfigWizard from "./routes/SellerConfigWizard.svelte";
  import SetupWizard from "./routes/SetupWizard.svelte";
  import TenantSettings from "./routes/TenantSettings.svelte";
  // PR-179 / session-179 — Outgoing / Incoming tab persistence on the
  // Invoices page. The tab is a top-level segmented control inside the
  // `invoices` route; the selection survives reloads via localStorage
  // (closed-vocab helper in `./lib/invoice-tab-persistence.ts`).
  import {
    loadInvoiceTab,
    saveInvoiceTab,
    type InvoiceTab,
  } from "./lib/invoice-tab-persistence";

  // PR-87 / session-112 — sessionStorage key the IssueInvoice route
  // uses to hand the just-issued invoice id back to InvoiceList on
  // navigation. Pre-PR-87 the IssueInvoice modal called `onIssued`
  // synchronously on the parent (InvoiceList), which then seeded its
  // local `navStack` to open the detail modal on the just-issued
  // invoice. Now that IssueInvoice is its own route, the post-issue
  // navigation back to `#/invoices` unmounts IssueInvoice before
  // InvoiceList mounts — sessionStorage bridges the two mounts
  // without widening the router to carry route-params (which the
  // tiny PR-53 router deliberately does NOT support). On the next
  // mount of InvoiceList, it reads + clears the key and seeds its
  // navStack; staleness is bounded by the tab lifetime.
  const JUST_ISSUED_KEY = "aberp:just-issued-invoice-id";

  // PR-53 / session-73 — hash-routing for the top-level navigation
  // shell. Three routes (`invoices` / `tenant` / `nav-credentials`);
  // the side-nav active item tracks `route`; deep-links into a
  // specific route work via the hash on first paint. The router only
  // takes effect once the backend reports Ready — pre-Ready, the
  // wizard chain owns the main pane (the operator can't usefully
  // navigate to settings without a session token).
  let route: AppRoute = $state(currentRoute());
  let unsubscribeRoute: (() => void) | null = null;

  // PR-179 / session-179 — Outgoing / Incoming tab state for the
  // `invoices` route. Outgoing (AR side, daily driver) is the
  // first-launch default; selection persists in localStorage so the
  // operator's last view survives a reload. The tab is scoped to the
  // `invoices` route only — switching to `partners` / `tenant` / etc.
  // leaves the tab untouched, and returning to `invoices` restores it.
  let invoicesTab: InvoiceTab = $state(loadInvoiceTab());

  function setInvoicesTab(next: InvoiceTab) {
    invoicesTab = next;
    saveInvoiceTab(next);
  }

  // PR-78 / session 101 — the flat NAV_ITEMS table was replaced by
  // the closed-vocab ERP module + AREA registry in
  // `./lib/erp-modules.ts` (per ADR-0041). The chrome groups
  // routes by USAGE FREQUENCY into two areas:
  //
  //   - "operational" = daily-driver workflow (Invoicing today;
  //     future Inventory / Accounting / Procurement). Front-and-
  //     center sidebar.
  //   - "maintenance" = configuration + master data, deliberately
  //     one level removed from the operational nav so it doesn't
  //     clutter the day-to-day. Entered via the topbar's gear
  //     affordance; the sidebar swaps to show maintenance modules
  //     only.
  //
  // `activeArea` derives from the current route's owning module's
  // area (defence-in-depth fallback to "operational" for unknown
  // routes, which `parseRoute` already filters into the default
  // `invoices` route). `activeSidebarModules` is the area-scoped
  // module list rendered in the sidebar. `activeModuleId` lights
  // up the parent header of the active route. Every existing hash
  // route still works verbatim; the only change is chrome
  // grouping + the area swap affordance.
  let activeArea: ErpArea = $derived(areaForRoute(route));
  let activeSidebarModules = $derived(modulesInArea(activeArea));
  let activeModuleId = $derived(moduleForRoute(route)?.id ?? null);

  // Click handler for the topbar's area-swap button. Navigates to
  // the default route of the *other* area (operational ↔
  // maintenance). PR-79 / session 102 elevated the maintenance
  // area's entry point from the first-module-first-route fall-
  // through (PR-78: `partners`) to its own landing dashboard at
  // `#/maintenance` — the operator now sees a glanceable tile grid
  // of master-data + settings before drilling in. Operational stays
  // bare per the roadmap Tier-3 pushback: the Invoice list IS the
  // daily-driver home, no dashboard widget set.
  function swapArea() {
    const target: ErpArea =
      activeArea === "operational" ? "maintenance" : "operational";
    const dest = defaultRouteForArea(target);
    if (dest !== null) navigateTo(dest);
  }

  // Boot-lifecycle gate state. We default to a `starting` snapshot
  // so the loading pane renders on the first paint without flashing
  // an empty/blank state.
  let bootSnapshot: BootStatusResponse = $state({
    status: "starting",
    error: null,
    recent_logs: [],
  });
  let viewMode: BootViewMode = $derived(bootViewMode(bootSnapshot.status));
  let bootPollTimer: ReturnType<typeof setInterval> | null = null;
  let healthPollTimer: ReturnType<typeof setInterval> | null = null;
  let retryInFlight = $state(false);

  // Post-boot /health probe — pre-PR-45a posture. Kept unchanged so
  // the header liveness dot stays honest after Ready: a backend that
  // crashes mid-session flips the dot to error and the operator
  // sees it. 10s matches the cold-start ceiling in
  // `backend::HANDSHAKE_TIMEOUT`; faster polling would be theatre
  // on a single-operator workstation (ADR-0017 §"ambient, never
  // theatrical").
  let healthState: "pending" | "ok" | "error" = $state("pending");
  let healthInfo: HealthResponse | null = $state(null);
  let healthError: string | null = $state(null);

  onMount(() => {
    void pollBoot();
    // 300ms cadence: fast enough that the loading-pane log line
    // looks like it's updating in near-real-time during cold boot,
    // slow enough that we're not hammering Tauri with invokes.
    bootPollTimer = setInterval(() => void pollBoot(), 300);
    unsubscribeRoute = subscribeRoute((r) => {
      route = r;
    });
  });

  onDestroy(() => {
    if (bootPollTimer !== null) clearInterval(bootPollTimer);
    if (healthPollTimer !== null) clearInterval(healthPollTimer);
    if (unsubscribeRoute !== null) unsubscribeRoute();
  });

  async function pollBoot() {
    try {
      const snap = await getBootStatus();
      bootSnapshot = snap;
      if (snap.status === "ready") {
        // Stop polling once we're Ready; switch to the existing
        // 10s health probe so the header dot stays honest.
        if (bootPollTimer !== null) {
          clearInterval(bootPollTimer);
          bootPollTimer = null;
        }
        if (healthPollTimer === null) {
          void probe();
          healthPollTimer = setInterval(() => void probe(), 10_000);
        }
      }
    } catch (err: unknown) {
      // A failed `get_boot_status` invoke is itself a Tauri-shell
      // issue, not a backend boot issue. Show it on the boot snapshot
      // so the operator sees something rather than a silent freeze.
      const message = err instanceof Error ? err.message : String(err);
      bootSnapshot = {
        status: "failed",
        error: `get_boot_status invoke failed: ${message}`,
        recent_logs: bootSnapshot.recent_logs,
      };
    }
  }

  async function probe() {
    try {
      healthInfo = await health();
      healthState = "ok";
      healthError = null;
    } catch (err: unknown) {
      healthState = "error";
      healthError = err instanceof Error ? err.message : String(err);
    }
  }

  // S166 — after the operator confirms the first-production launch,
  // re-probe /health so `first_prod_launch_required` flips to false and
  // the FirstProdLaunchModal unmounts, revealing the normal app.
  async function onFirstProdLaunchAcknowledged() {
    await probe();
  }

  async function onRetryClick() {
    retryInFlight = true;
    try {
      await retryBoot();
      // Restart the boot-poll cadence so the loading pane renders
      // again immediately. The Rust side resets the boot_state to
      // `starting` inside `boot_backend`, so the next poll picks up
      // the in-flight lifecycle.
      bootSnapshot = {
        status: "starting",
        error: null,
        recent_logs: [],
      };
      if (bootPollTimer === null) {
        bootPollTimer = setInterval(() => void pollBoot(), 300);
      }
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      bootSnapshot = {
        status: "failed",
        error: `retry_boot invoke failed: ${message}`,
        recent_logs: bootSnapshot.recent_logs,
      };
    } finally {
      retryInFlight = false;
    }
  }

  let bootErr = $derived(bootErrorMessage(bootSnapshot));
  let latestLog = $derived(latestLogLine(bootSnapshot));

  // S166 — block the main routes behind the one-time first-production-
  // launch confirmation while `/health` reports it as required. Always
  // false on dev/test builds (the backend reports false there).
  let firstProdLaunchRequired = $derived(shouldShowFirstProdLaunchModal(healthInfo));
</script>

<div class="frame">
  <header class="topbar">
    <h1 class="wordmark">ABERP</h1>
    {#if viewMode === "ready"}
      <!-- PR-78 / session 101 — area-swap affordance per ADR-0041
           §3. When operating in the daily workflow, a small "⚙
           Maintenance" button sits in the topbar; clicking it
           navigates to the maintenance area's default route
           (Partners today). When in maintenance, the button flips
           to "← Operational" returning to Invoices. Deliberately
           understated (small, secondary-text, no badge counts) —
           the design language is ambient, not theatrical
           (ADR-0017). -->
      <button
        type="button"
        class="area-swap"
        data-target={activeArea === "operational" ? "maintenance" : "operational"}
        onclick={swapArea}
        title={activeArea === "operational"
          ? `Open ${AREA_LABELS.maintenance.en} (master data + settings)`
          : `Back to ${AREA_LABELS.operational.en} workflow`}
      >
        {#if activeArea === "operational"}
          <span class="area-swap__glyph" aria-hidden="true">⚙</span>
          <span class="area-swap__label">{AREA_LABELS.maintenance.en}</span>
        {:else}
          <span class="area-swap__glyph" aria-hidden="true">←</span>
          <span class="area-swap__label">{AREA_LABELS.operational.en}</span>
        {/if}
      </button>
      <div
        class="status"
        data-state={healthState}
        title={healthInfo
          ? `binary ${healthInfo.binary_hash.slice(0, 12)}… · NAV XSD ${healthInfo.nav_xsd_version}`
          : (healthError ?? "")}
      >
        <span class="dot" aria-hidden="true"></span>
        <span class="label">
          {#if healthState === "ok" && healthInfo}
            backend ok · NAV XSD {healthInfo.nav_xsd_version}
          {:else if healthState === "pending"}
            probing backend…
          {:else}
            backend unreachable
          {/if}
        </span>
      </div>
    {:else if viewMode === "loading"}
      <div class="status" data-state="pending">
        <span class="dot" aria-hidden="true"></span>
        <span class="label">backend starting…</span>
      </div>
    {:else if viewMode === "setup"}
      <div class="status" data-state="pending">
        <span class="dot" aria-hidden="true"></span>
        <span class="label">first-run setup</span>
      </div>
    {:else if viewMode === "seller-config"}
      <div class="status" data-state="pending">
        <span class="dot" aria-hidden="true"></span>
        <span class="label">seller setup</span>
      </div>
    {:else}
      <div class="status" data-state="error">
        <span class="dot" aria-hidden="true"></span>
        <span class="label">backend boot failed</span>
      </div>
    {/if}
  </header>

  {#if viewMode === "ready"}
    {#if firstProdLaunchRequired}
      <!-- S166 — block every main route behind the one-time
           first-production-launch confirmation until the operator
           acknowledges. The modal POSTs the acknowledgement, then
           `onFirstProdLaunchAcknowledged` re-probes /health, flipping
           the flag false and unmounting this branch. -->
      <FirstProdLaunchModal onAcknowledged={onFirstProdLaunchAcknowledged} />
    {:else}
    <div class="layout">
      <nav
        class="sidenav"
        aria-label={AREA_LABELS[activeArea].en}
        data-area={activeArea}
      >
        <!-- Area-section caption sits at the top of the sidebar so
             the operator knows which area they're in (especially
             important the first time they hit the gear and the
             sidebar contents change). Quiet chrome — not a
             clickable target, not a section header in the
             marketing-deck sense. -->
        <div class="sidenav__area-caption">
          <span class="sidenav__area-caption-label">
            {AREA_LABELS[activeArea].en}
          </span>
        </div>
        <ul class="sidenav__modules">
          {#each activeSidebarModules as mod (mod.id)}
            <li
              class="sidenav__module"
              class:sidenav__module--active={activeModuleId === mod.id}
            >
              <!-- Module header is presentational, not a navigation
                   target — modules group routes, they aren't routes
                   themselves (ADR-0041 §3). `aria-hidden="true"` on
                   the glyph keeps the screen-reader output clean
                   ("Invoicing" not "section sign Invoicing"). -->
              <div class="sidenav__module-header">
                <span class="sidenav__module-glyph" aria-hidden="true">
                  {mod.glyph}
                </span>
                <span class="sidenav__module-label">{mod.label_en}</span>
              </div>
              <ul class="sidenav__routes">
                <!-- PR-86 / session-111 — skip routes flagged
                     `hidden: true` (e.g. `invoices-new`, which is
                     reached via the "+ New invoice" button on the
                     list rather than via a sidebar row). The route
                     is still registered under its owning module so
                     `areaForRoute` resolves correctly; only the
                     sidebar rendering hides it. -->
                {#each mod.routes.filter((r) => !r.hidden) as r (r.id)}
                  <li>
                    <a
                      class="sidenav__item"
                      href={routeHash(r.id)}
                      aria-current={route === r.id ? "page" : undefined}
                      onclick={(e) => {
                        // The native `<a>` href on a hash link
                        // already pushes to history; calling
                        // navigateTo here is belt-and-suspenders for
                        // any test environment (vitest jsdom) that
                        // doesn't fire hashchange.
                        e.preventDefault();
                        navigateTo(r.id);
                      }}
                    >
                      {r.label}
                    </a>
                  </li>
                {/each}
              </ul>
            </li>
          {/each}
        </ul>
      </nav>
      <main class="main">
        {#if healthState === "error"}
          <section class="banner" role="alert">
            <strong>Backend is not responding.</strong>
            <p class="banner-detail">{healthError}</p>
            <p class="banner-hint">
              Run <code>aberp serve --tenant default</code> in a terminal at least
              once so the session token is minted in the OS keychain, then
              relaunch this shell.
            </p>
          </section>
        {/if}
        {#if route === "tenant"}
          <TenantSettings
            isProductionBuild={healthInfo?.is_production_build ?? false}
          />
        {:else if route === "nav-credentials"}
          <NavCredentialsSettings />
        {:else if route === "partners"}
          <PartnersList />
        {:else if route === "products"}
          <ProductsList />
        {:else if route === "maintenance"}
          <MaintenanceDashboard />
        {:else if route === "invoices-new"}
          <!-- PR-87 / session-112 — full-page issuance route. The
               IssueInvoice form was a `<dialog>` modal mounted inside
               InvoiceList pre-PR-87 (PR-86 enlarged the modal which
               Ervin declined — he asked explicitly for full-page SPA
               navigation so the app becomes more portable). The form
               now lives here as a routable surface. On success, stash
               the just-issued invoice id in sessionStorage + navigate
               back to `#/invoices`; the list reads the stash on
               mount and opens the detail modal on that id. On cancel
               (button or ESC), navigate back without stashing. -->
          <section class="issue-page" aria-labelledby="issue-page-title">
            <header class="issue-page__head">
              <h2 id="issue-page-title">New invoice</h2>
              <p class="issue-page__hint">
                Review every field — buyer, currency, dates, bank, lines, notes,
                totals — before pressing "Issue invoice". The issuance writes
                to the regulatory audit ledger and submits to NAV.
              </p>
            </header>
            <IssueInvoice
              onIssued={(invoiceId) => {
                try {
                  sessionStorage.setItem(JUST_ISSUED_KEY, invoiceId);
                } catch (_e) {
                  // sessionStorage can throw in private-browsing or
                  // quota-full contexts; the navigation still
                  // completes — the operator just doesn't get the
                  // auto-open-detail affordance on landing back at
                  // the list. CLAUDE.md rule 12 — fail loud at the
                  // store boundary, but don't gate navigation.
                  console.warn("could not stash just-issued invoice id", _e);
                }
                navigateTo("invoices");
              }}
              onClose={() => navigateTo("invoices")}
            />
          </section>
        {:else}
          <!-- PR-179 / session-179 — Outgoing / Incoming tab split on
               the Invoices page. The two tabs share the same `#/invoices`
               route (the tiny hash router does not carry sub-params);
               the tab state is local SPA state, persisted to
               localStorage so the operator's view survives a reload.
               Outgoing (AR, daily driver) is default. -->
          <div class="invoices-tabs" role="tablist" aria-label="Számlák / Invoices">
            <button
              type="button"
              role="tab"
              class="invoices-tab"
              class:invoices-tab--active={invoicesTab === "outgoing"}
              aria-selected={invoicesTab === "outgoing"}
              onclick={() => setInvoicesTab("outgoing")}
            >
              <span class="invoices-tab__label">Kimenő</span>
              <span class="invoices-tab__sub">Outgoing</span>
            </button>
            <button
              type="button"
              role="tab"
              class="invoices-tab"
              class:invoices-tab--active={invoicesTab === "incoming"}
              aria-selected={invoicesTab === "incoming"}
              onclick={() => setInvoicesTab("incoming")}
            >
              <span class="invoices-tab__label">Bejövő</span>
              <span class="invoices-tab__sub">Incoming</span>
            </button>
          </div>
          {#if invoicesTab === "incoming"}
            <IncomingInvoiceList />
          {:else}
            <InvoiceList />
          {/if}
        {/if}
      </main>
    </div>
    {/if}
  {:else}
  <main>
    {#if viewMode === "setup"}
      <SetupWizard />
    {:else if viewMode === "seller-config"}
      <SellerConfigWizard />
    {:else if viewMode === "loading"}
      <section class="boot-pane boot-pane--loading" role="status" aria-live="polite">
        <div class="boot-pane__spinner" aria-hidden="true"></div>
        <h2 class="boot-pane__title">Starting backend…</h2>
        <p class="boot-pane__line">
          {#if latestLog !== null}
            {latestLog}
          {:else}
            Spawning <code>aberp serve</code>…
          {/if}
        </p>
        {#if bootSnapshot.recent_logs.length > 0}
          <details class="boot-pane__details">
            <summary>Recent backend log lines</summary>
            <ol class="boot-pane__log">
              {#each bootSnapshot.recent_logs as logLine, i (i)}
                <li>{logLine}</li>
              {/each}
            </ol>
          </details>
        {/if}
      </section>
    {:else if viewMode === "error"}
      <section class="boot-pane boot-pane--error" role="alert">
        <h2 class="boot-pane__title">Backend boot failed</h2>
        <p class="boot-pane__detail">{bootErr}</p>
        <div class="boot-pane__actions">
          <button
            class="boot-pane__retry"
            type="button"
            onclick={() => void onRetryClick()}
            disabled={retryInFlight}
          >
            {retryInFlight ? "Retrying…" : "Retry"}
          </button>
        </div>
        <details class="boot-pane__details" open>
          <summary>Common causes</summary>
          <ul class="boot-pane__hints">
            {#each FAILURE_HINTS as hint, i (i)}
              <li>{hint}</li>
            {/each}
          </ul>
        </details>
        {#if bootSnapshot.recent_logs.length > 0}
          <details class="boot-pane__details">
            <summary>Recent backend log lines</summary>
            <ol class="boot-pane__log">
              {#each bootSnapshot.recent_logs as logLine, i (i)}
                <li>{logLine}</li>
              {/each}
            </ol>
          </details>
        {/if}
      </section>
    {/if}
  </main>
  {/if}
</div>

<style>
  .frame {
    display: flex;
    flex-direction: column;
    min-height: 100vh;
  }

  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
    padding: var(--space-3) var(--space-5);
    background: var(--color-surface-raised);
    border-bottom: 1px solid var(--color-surface-divider);
  }

  /* The wordmark stays the left-most anchor; the area-swap button
   * and the backend-status pill sit on the right. Push the right-
   * hand cluster to the end of the row. */
  .topbar > .area-swap {
    margin-left: auto;
  }

  /* PR-78 / session 101 — area-swap affordance per ADR-0041 §3.
   * The button sits in the topbar as a small, secondary control;
   * it is NOT the visual focal point. Quiet chrome posture per
   * ADR-0017. The button label changes between "Maintenance" (when
   * operating) and "Operational" (when in maintenance), so the
   * operator always sees the destination of the next click. */
  .area-swap {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-3);
    background: transparent;
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    color: var(--color-text-secondary);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    cursor: pointer;
  }

  .area-swap:hover {
    color: var(--color-text-strong);
    background: var(--color-surface-divider);
  }

  /* When the operator is IN the maintenance area, the swap-back
   * button gets a slightly stronger border so it reads as the
   * primary way out. Not theatrical — just enough visual weight
   * that a new operator immediately spots the way back. */
  .area-swap[data-target="operational"] {
    color: var(--color-text-strong);
  }

  .area-swap__glyph {
    display: inline-block;
    width: 12px;
    text-align: center;
  }

  .area-swap__label {
    line-height: 1;
  }

  .wordmark {
    margin: 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-lg);
    font-weight: 600;
    letter-spacing: 0.06em;
    color: var(--color-text-strong);
  }

  .status {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--color-signal-muted);
  }

  .status[data-state="ok"] .dot {
    background: var(--color-signal-positive);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .status[data-state="error"] .dot {
    background: var(--color-signal-negative);
  }

  .status[data-state="pending"] .dot {
    background: var(--color-signal-muted);
    animation: aberp-pulse 1.4s ease-in-out infinite;
  }

  main {
    flex: 1;
    padding: var(--space-5);
    overflow: auto;
  }

  /* PR-53 / session-73 — top-level layout with the side-nav (left)
   * and the route's main pane (right). Two-column grid; the side-nav
   * carries its own background to read as chrome against the
   * existing dark theme. */
  .layout {
    flex: 1;
    display: grid;
    grid-template-columns: 220px 1fr;
    min-height: 0;
  }

  .sidenav {
    background: var(--color-surface-raised);
    border-right: 1px solid var(--color-surface-divider);
    padding: var(--space-4) 0;
  }

  /* PR-78 + PR-79 — the maintenance area gets a distinct surface so
   * the operator immediately recognises "I am in the configuration
   * area, not in my daily workflow". PR-78 shipped subtle (one
   * surface step); PR-79 bumps it one notch — uses the sunken
   * surface (darker than operational's raised) so the chrome reads
   * as visibly a different space without crossing into "different
   * app". Pair with the area-caption accent stripe below for the
   * "you are here" cue at glance. */
  .sidenav[data-area="maintenance"] {
    background: var(--color-surface-sunken, var(--color-surface-base));
  }

  /* Area caption at the top of the sidebar. Tells the operator
   * which area they're in. Presentational only — not a nav
   * target. */
  .sidenav__area-caption {
    padding: 0 var(--space-4) var(--space-3) var(--space-4);
    border-bottom: 1px solid var(--color-surface-divider);
    margin-bottom: var(--space-3);
  }

  .sidenav__area-caption-label {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--color-text-strong);
  }

  /* PR-79 — area-caption accent stripe in maintenance mode. A short
   * left-edge bar in the warning color gives the "you are not in
   * your daily workflow" cue at glance. Absent in operational so
   * the daily-driver chrome stays unaffected. One CSS rule. */
  .sidenav[data-area="maintenance"] .sidenav__area-caption {
    border-left: 3px solid var(--color-signal-warning);
    padding-left: calc(var(--space-4) - 3px);
  }

  /* PR-78 / session 101 — two-level sidebar (ADR-0041 §3). Outer
   * list groups by ERP module; each module header is presentational
   * (glyph + label, no click handler), its nested `.sidenav__routes`
   * carries the actual `<a>` rows. The route-row chrome below is
   * unchanged from PR-53 — preserving the active-item visual + the
   * `aria-current="page"` indicator that the keyboard nav (PR-68)
   * and screen readers rely on. */
  .sidenav__modules {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .sidenav__module {
    display: flex;
    flex-direction: column;
  }

  .sidenav__module-header {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-4);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-muted);
    font-family: var(--type-family-mono);
  }

  /* Parent-module-of-active-route is marked subtly: brighter label
   * colour, the glyph lit. The route row itself still carries the
   * `aria-current="page"` highlight; this is just the "you are in
   * this section" cue at the module header level. */
  .sidenav__module--active .sidenav__module-header {
    color: var(--color-text-strong);
  }

  .sidenav__module-glyph {
    display: inline-block;
    width: 14px;
    text-align: center;
    color: var(--color-text-muted);
  }

  .sidenav__module--active .sidenav__module-glyph {
    color: var(--color-signal-positive, var(--color-text-strong));
  }

  .sidenav__module-label {
    line-height: 1;
  }

  .sidenav__routes {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
  }

  .sidenav__item {
    display: block;
    padding: var(--space-2) var(--space-4) var(--space-2) var(--space-6);
    color: var(--color-text-secondary);
    text-decoration: none;
    font-size: var(--type-size-sm);
    border-left: 3px solid transparent;
  }

  .sidenav__item:hover {
    color: var(--color-text-strong);
    background: var(--color-surface-divider);
  }

  .sidenav__item[aria-current="page"] {
    color: var(--color-text-strong);
    border-left-color: var(--color-signal-positive, var(--color-text-strong));
    background: var(--color-surface-divider);
    font-weight: 500;
  }

  .main {
    padding: var(--space-5);
    overflow: auto;
  }

  /* PR-87 / session-112 — full-page Issue Invoice route chrome. The
   * `<IssueInvoice>` form owns its own .issue-frame stack-of-fieldsets
   * layout; the page chrome here adds a quiet title bar + hint line so
   * the operator immediately knows what surface they're on (and so a
   * deep-link / back-button arrival from elsewhere lands with context).
   * Centred max-width matches the form's own 960px cap so the title
   * sits over the same column. */
  .issue-page {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .issue-page__head {
    max-width: 960px;
    margin: 0 auto;
    width: 100%;
    padding-bottom: var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
  }

  .issue-page__head h2 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .issue-page__hint {
    margin: var(--space-2) 0 0 0;
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }

  .banner {
    margin-bottom: var(--space-5);
    padding: var(--space-3) var(--space-4);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    font-size: var(--type-size-sm);
  }

  .banner-detail {
    margin: var(--space-2) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .banner-hint {
    margin: var(--space-2) 0 0 0;
    color: var(--color-text-muted);
  }

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  .boot-pane {
    max-width: 720px;
    margin: var(--space-5) auto;
    padding: var(--space-5);
    background: var(--color-surface-raised);
    border-radius: 6px;
    border: 1px solid var(--color-surface-divider);
  }

  .boot-pane--error {
    border-left: 3px solid var(--color-signal-negative);
  }

  .boot-pane__title {
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-md);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .boot-pane__line {
    margin: 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .boot-pane__detail {
    margin: 0 0 var(--space-3) 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .boot-pane__spinner {
    width: 16px;
    height: 16px;
    margin: 0 0 var(--space-3) 0;
    border-radius: 50%;
    border: 2px solid var(--color-surface-divider);
    border-top-color: var(--color-signal-muted);
    animation: aberp-spin 1s linear infinite;
  }

  .boot-pane__actions {
    margin: var(--space-3) 0 var(--space-4) 0;
  }

  .boot-pane__retry {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border-radius: 4px;
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .boot-pane__retry:disabled {
    opacity: 0.6;
    cursor: progress;
  }

  .boot-pane__retry:hover:not(:disabled) {
    background: var(--color-surface-divider);
  }

  .boot-pane__details {
    margin-top: var(--space-3);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }

  .boot-pane__details summary {
    cursor: pointer;
    color: var(--color-text-muted);
  }

  .boot-pane__hints {
    margin: var(--space-2) 0 0 var(--space-3);
    padding: 0 0 0 var(--space-3);
    list-style: disc;
  }

  .boot-pane__hints li {
    margin-bottom: var(--space-1);
  }

  .boot-pane__log {
    margin: var(--space-2) 0 0 0;
    padding: 0;
    list-style: none;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .boot-pane__log li {
    padding: 2px 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  @keyframes aberp-spin {
    from {
      transform: rotate(0deg);
    }
    to {
      transform: rotate(360deg);
    }
  }

  @keyframes aberp-pulse {
    0%,
    100% {
      opacity: 0.4;
    }
    50% {
      opacity: 1;
    }
  }

  /* PR-179 / session-179 — Outgoing / Incoming segmented control sits
   * above the dense table. Quiet chrome per ADR-0017: no fill, no
   * accent colour, just a stronger underline on the active tab. The
   * Hungarian primary label and English secondary label stack so the
   * tab reads bilingually without doubling the width. */
  .invoices-tabs {
    display: flex;
    gap: var(--space-1);
    border-bottom: 1px solid var(--color-surface-divider);
    margin-bottom: var(--space-4);
  }

  .invoices-tab {
    display: inline-flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 0;
    background: none;
    border: none;
    padding: var(--space-2) var(--space-4);
    margin-bottom: -1px;
    border-bottom: 2px solid transparent;
    color: var(--color-text-secondary);
    cursor: pointer;
    font-family: inherit;
  }

  .invoices-tab:hover {
    color: var(--color-text-strong);
  }

  .invoices-tab--active {
    color: var(--color-text-strong);
    border-bottom-color: var(--color-text-strong);
  }

  .invoices-tab__label {
    font-size: var(--type-size-md);
    font-weight: 500;
    letter-spacing: 0.02em;
  }

  .invoices-tab__sub {
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--color-text-muted);
  }

  .invoices-tab--active .invoices-tab__sub {
    color: var(--color-text-secondary);
  }
</style>
