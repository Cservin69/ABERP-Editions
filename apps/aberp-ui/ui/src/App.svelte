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
  import InvoiceList from "./routes/InvoiceList.svelte";
  import SellerConfigWizard from "./routes/SellerConfigWizard.svelte";
  import SetupWizard from "./routes/SetupWizard.svelte";

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
  });

  onDestroy(() => {
    if (bootPollTimer !== null) clearInterval(bootPollTimer);
    if (healthPollTimer !== null) clearInterval(healthPollTimer);
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
</script>

<div class="frame">
  <header class="topbar">
    <h1 class="wordmark">ABERP</h1>
    {#if viewMode === "ready"}
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
    {:else}
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
      <InvoiceList />
    {/if}
  </main>
</div>

<style>
  .frame {
    display: flex;
    flex-direction: column;
    min-height: 100vh;
  }

  .topbar {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    padding: var(--space-3) var(--space-5);
    background: var(--color-surface-raised);
    border-bottom: 1px solid var(--color-surface-divider);
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
</style>
