<script lang="ts">
  // S429 — read-only closed-loop calibration page. Per-family coefficient +
  // last-N samples chart + recent skips (WOs that lost calibration signal).
  // Calibration is COMPUTED, never operator-tuned — there is deliberately no
  // edit control here ([[trust-code-not-operator]]). Manual Refresh; no
  // real-time (the signal arrives on WO-Complete, not continuously).
  import { onMount } from "svelte";
  import { getCalibration } from "../lib/api";
  import {
    coefficientChipClass,
    coefficientHint,
    formatCoefficient,
    normalizeOverview,
    sortFamilies,
    sparklineBars,
    type CalibrationOverview,
  } from "../lib/calibration";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let data = $state<CalibrationOverview | null>(null);

  const families = $derived(sortFamilies(data?.families ?? []));
  const skips = $derived(data?.recent_skips ?? []);

  async function load() {
    loadState = "loading";
    errorMessage = null;
    try {
      data = normalizeOverview(await getCalibration());
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  onMount(load);
</script>

<section class="calibration">
  <header class="head">
    <div>
      <h1>Árazási kalibráció / Pricing calibration</h1>
      <p class="sub">
        A megmunkálási idő becslés gépcsaládonkénti korrekciója a tényleges
        munkaidők alapján. Csak olvasható — számított, nem szerkeszthető.
        <br />
        Per-family correction of the machining-time estimate, learned from
        actual job times. Read-only — computed, not editable.
      </p>
    </div>
    <button class="refresh" onclick={load} disabled={loadState === "loading"}>
      {loadState === "loading" ? "Frissítés…" : "Frissítés / Refresh"}
    </button>
  </header>

  {#if data?.coefficient_set_hash}
    <p class="hash">
      Aktív együttható-készlet / active coefficient set:
      <code>{data.coefficient_set_hash}</code>
    </p>
  {/if}

  {#if loadState === "error"}
    <p class="error">Hiba / Error: {errorMessage}</p>
  {:else if loadState === "ready" && families.length === 0}
    <p class="empty">
      Még nincs kalibrációs minta. Egy minta akkor keletkezik, amikor egy
      árajánlathoz kötött munkalapot rögzített megmunkálási idővel zárnak le.
      <br />
      No calibration samples yet. A sample is recorded when a work order linked
      to a quote is Completed with a recorded actual machining time.
    </p>
  {:else if loadState === "ready"}
    <div class="families">
      {#each families as fam (fam.machine_family)}
        {@const bars = sparklineBars(fam.samples)}
        <article class="family-card">
          <div class="family-head">
            <h2>{fam.machine_family}</h2>
            <span
              class="chip {coefficientChipClass(fam.coefficient)}"
              title={coefficientHint(fam.coefficient)}
            >
              {formatCoefficient(fam.coefficient)}
            </span>
            <span class="count">{fam.sample_count} minta / samples</span>
          </div>

          {#if bars.length > 0}
            <svg
              class="spark"
              viewBox="0 0 {Math.max(bars.length * 14, 14)} 60"
              preserveAspectRatio="none"
              role="img"
              aria-label="{fam.machine_family} estimated vs actual"
            >
              {#each bars as bar, i (i)}
                <rect
                  class="bar-est"
                  x={i * 14 + 1}
                  y={56 - bar.estimatedFraction * 52}
                  width="5"
                  height={bar.estimatedFraction * 52}
                />
                <rect
                  class="bar-act"
                  x={i * 14 + 7}
                  y={56 - bar.actualFraction * 52}
                  width="5"
                  height={bar.actualFraction * 52}
                />
              {/each}
            </svg>
            <div class="legend">
              <span><i class="sw sw-est"></i> becsült / estimated</span>
              <span><i class="sw sw-act"></i> tényleges / actual</span>
            </div>
          {/if}
        </article>
      {/each}
    </div>

    <section class="skips">
      <h2>Kihagyott minták / Recent skips</h2>
      <p class="sub">
        Munkalapok, amelyek elvesztették a kalibrációs jelet (nincs rögzített
        idő, vagy nincs árazott árajánlat). Work orders that lost calibration
        signal.
      </p>
      {#if skips.length === 0}
        <p class="empty-small">Nincs kihagyás / No skips.</p>
      {:else}
        <table>
          <thead>
            <tr>
              <th>Időpont / At</th>
              <th>Árajánlat / Quote</th>
              <th>Munkalap / WO</th>
              <th>Ok / Reason</th>
            </tr>
          </thead>
          <tbody>
            {#each skips as skip (skip.work_order_id + skip.at_utc)}
              <tr>
                <td class="mono">{skip.at_utc}</td>
                <td class="mono">{skip.quote_id}</td>
                <td class="mono">{skip.work_order_id}</td>
                <td>{skip.reason}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>
  {/if}
</section>

<style>
  .calibration {
    padding: 1.25rem 1.5rem;
    color: var(--color-text, #e5e7eb);
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 1rem;
  }
  h1 {
    font-size: 1.25rem;
    margin: 0 0 0.25rem;
  }
  h2 {
    font-size: 1rem;
    margin: 0;
  }
  .sub {
    margin: 0.25rem 0 0;
    font-size: 0.8rem;
    color: var(--color-muted, #9ca3af);
    line-height: 1.4;
  }
  .refresh {
    background: var(--color-surface-2, #374151);
    color: var(--color-text, #e5e7eb);
    border: 1px solid var(--color-border, #4b5563);
    border-radius: var(--radius-md);
    padding: 0.4rem 0.8rem;
    cursor: pointer;
    white-space: nowrap;
  }
  .refresh:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .hash {
    font-size: 0.75rem;
    color: var(--color-muted, #9ca3af);
    margin: 0.75rem 0 0;
  }
  .hash code,
  .mono {
    font-family: ui-monospace, monospace;
    font-size: 0.75rem;
  }
  .error {
    color: var(--color-danger, #f87171);
    margin-top: 1rem;
  }
  .empty,
  .empty-small {
    color: var(--color-muted, #9ca3af);
    font-size: 0.85rem;
    line-height: 1.5;
    margin-top: 1rem;
  }
  .families {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
    gap: 1rem;
    margin-top: 1.25rem;
  }
  .family-card {
    background: var(--color-surface, #1f2937);
    border: 1px solid var(--color-border, #374151);
    border-radius: var(--radius-md);
    padding: 0.9rem;
  }
  .family-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
  }
  .family-head h2 {
    font-family: ui-monospace, monospace;
    font-size: 0.9rem;
  }
  .count {
    margin-left: auto;
    font-size: 0.7rem;
    color: var(--color-muted, #9ca3af);
  }
  .chip {
    font-size: 0.75rem;
    font-weight: 600;
    padding: 0.1rem 0.45rem;
    border-radius: var(--radius-pill);
    border: 1px solid transparent;
  }
  .chip-neutral {
    background: #1f2937;
    color: #9ca3af;
    border-color: #4b5563;
  }
  .chip-under {
    background: rgba(34, 197, 94, 0.15);
    color: #4ade80;
    border-color: rgba(34, 197, 94, 0.4);
  }
  .chip-under-strong {
    background: rgba(34, 197, 94, 0.25);
    color: #86efac;
    border-color: #22c55e;
  }
  .chip-over {
    background: rgba(245, 158, 11, 0.15);
    color: #fbbf24;
    border-color: rgba(245, 158, 11, 0.4);
  }
  .chip-over-strong {
    background: rgba(239, 68, 68, 0.2);
    color: #f87171;
    border-color: #ef4444;
  }
  .spark {
    width: 100%;
    height: 60px;
    margin-top: 0.75rem;
    display: block;
  }
  .bar-est {
    fill: #60a5fa;
  }
  .bar-act {
    fill: #f472b6;
  }
  .legend {
    display: flex;
    gap: 1rem;
    margin-top: 0.4rem;
    font-size: 0.7rem;
    color: var(--color-muted, #9ca3af);
  }
  .sw {
    display: inline-block;
    width: 9px;
    height: 9px;
    border-radius: var(--radius-sm);
    margin-right: 2px;
    vertical-align: middle;
  }
  .sw-est {
    background: #60a5fa;
  }
  .sw-act {
    background: #f472b6;
  }
  .skips {
    margin-top: 1.75rem;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    margin-top: 0.75rem;
    font-size: 0.8rem;
  }
  th,
  td {
    text-align: left;
    padding: 0.4rem 0.6rem;
    border-bottom: 1px solid var(--color-border, #374151);
  }
  th {
    color: var(--color-muted, #9ca3af);
    font-weight: 600;
  }
</style>
