<script lang="ts">
  // S426 / ADR-0082 — operator Snapshots tab. Lists validated logical DB
  // snapshots, lets the operator take one now, and runs a GUARDED restore.
  // The 2026-06-11 ART-corruption defence made operator-visible.
  //
  // Real-time: NO. Manual Refresh + "snapshot now". The periodic daemon
  // (every 4h) does the unattended work; this surface is for inspection +
  // on-demand action. Restore is deliberately high-friction: confirm
  // checkbox + a target that the backend refuses if it is a live ~/.aberp
  // DB ([[trust-code-not-operator]] — the safety is in the binary; this UI
  // mirrors it for early feedback).
  import { onMount } from "svelte";
  import {
    listSnapshots,
    snapshotNow,
    restoreSnapshot,
    type SnapshotsListResponse,
  } from "../lib/api";
  import {
    canSubmitRestore,
    filterSnapshots,
    restoreTargetWarning,
    sortSnapshots,
    summarizeSnapshots,
    type SnapshotSortKey,
    type SnapshotStatusFacet,
    type SortDir,
  } from "../lib/snapshots-list";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let data = $state<SnapshotsListResponse | null>(null);

  let statusFacet = $state<SnapshotStatusFacet>("all");
  let sort = $state<{ key: SnapshotSortKey; dir: SortDir }>({ key: "seq", dir: "desc" });

  // "Snapshot now" action state.
  let snapBusy = $state(false);
  let snapMessage = $state<string | null>(null);

  // Restore wizard state.
  let restoreOpen = $state(false);
  let restoreSelector = $state("");
  let restoreTarget = $state("");
  let restoreConfirm = $state(false);
  let restoreBusy = $state(false);
  let restoreMessage = $state<string | null>(null);
  let restoreError = $state<string | null>(null);

  const rows = $derived(data?.snapshots ?? []);
  const summary = $derived(summarizeSnapshots(rows));
  const visibleRows = $derived(
    sortSnapshots(filterSnapshots(rows, statusFacet), sort.key, sort.dir),
  );
  const targetWarning = $derived(restoreTargetWarning(restoreTarget));
  const canSubmit = $derived(
    canSubmitRestore({
      selector: restoreSelector,
      to: restoreTarget,
      confirm: restoreConfirm,
    }),
  );

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      data = await listSnapshots();
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  async function takeSnapshotNow(): Promise<void> {
    if (snapBusy) return;
    snapBusy = true;
    snapMessage = null;
    errorMessage = null;
    try {
      const resp = await snapshotNow();
      const c = resp.created;
      snapMessage = c.valid
        ? `Pillanatkép #${c.seq} elkészült és érvényes (${c.size_human}). / Snapshot #${c.seq} created and valid.`
        : `Pillanatkép #${c.seq} ELBUKOTT a validáción (megőrizve). / Snapshot #${c.seq} FAILED validation (kept).`;
      await refresh();
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
    } finally {
      snapBusy = false;
    }
  }

  function openRestore(seq: number): void {
    restoreSelector = String(seq);
    restoreTarget = "";
    restoreConfirm = false;
    restoreMessage = null;
    restoreError = null;
    restoreOpen = true;
  }

  async function submitRestore(): Promise<void> {
    if (!canSubmit || restoreBusy) return;
    restoreBusy = true;
    restoreMessage = null;
    restoreError = null;
    try {
      const resp = await restoreSnapshot(
        restoreSelector.trim(),
        restoreTarget.trim(),
        restoreConfirm,
      );
      restoreMessage = `Visszaállítva #${resp.restored_seq} → ${resp.target}. Ellenőrizd, állítsd le a szervert, majd cseréld be. / Restored #${resp.restored_seq} → ${resp.target}. Verify, stop serve, then swap it in.`;
    } catch (e) {
      restoreError = e instanceof Error ? e.message : String(e);
    } finally {
      restoreBusy = false;
    }
  }

  function setSort(key: SnapshotSortKey): void {
    if (sort.key === key) {
      sort = { key, dir: sort.dir === "asc" ? "desc" : "asc" };
    } else {
      sort = { key, dir: "desc" };
    }
  }

  function sortIndicator(key: SnapshotSortKey): string {
    if (sort.key !== key) return "";
    return sort.dir === "asc" ? "▲" : "▼";
  }

  const FACETS: { id: SnapshotStatusFacet; label: string }[] = [
    { id: "all", label: "Mind / All" },
    { id: "valid", label: "Érvényes / Valid" },
    { id: "invalid", label: "Hibás / Invalid" },
  ];
</script>

<section class="snap">
  <header class="snap__head">
    <div class="snap__title">
      <h1>🛟 Pillanatképek / Snapshots</h1>
      <p class="snap__sub">
        Érvényesített logikai DuckDB pillanatképek (ADR-0082). / Validated
        logical DuckDB snapshots.
      </p>
    </div>
    <div class="snap__actions">
      <button
        type="button"
        class="snap__btn snap__btn--primary"
        onclick={takeSnapshotNow}
        disabled={snapBusy}
        data-testid="snapshot-now"
      >
        {snapBusy ? "Folyamatban… / Working…" : "📸 Pillanatkép most / Snapshot now"}
      </button>
      <button type="button" class="snap__btn" onclick={refresh} disabled={loadState === "loading"}>
        ⟳ Frissítés / Refresh
      </button>
    </div>
  </header>

  {#if data}
    <div class="snap__banner" data-testid="snapshot-summary">
      <span><strong>{summary.total}</strong> összesen / total</span>
      <span class="ok"><strong>{summary.valid}</strong> érvényes / valid</span>
      {#if summary.invalid > 0}
        <span class="bad"><strong>{summary.invalid}</strong> hibás / invalid</span>
      {/if}
      {#if summary.newest_valid_seq !== null}
        <span>legutóbbi jó / newest good: <strong>#{summary.newest_valid_seq}</strong></span>
      {/if}
      <span class="snap__store" title={data.store_dir}>📁 {data.store_dir}</span>
      {#if data.daemon_disabled}
        <span class="bad">⏸ démon kikapcsolva / daemon disabled</span>
      {:else}
        <span>⏱ {Math.round(data.interval_secs / 3600)}h ütem / cadence</span>
      {/if}
    </div>
  {/if}

  {#if snapMessage}
    <p class="snap__msg ok" data-testid="snapshot-now-msg">{snapMessage}</p>
  {/if}
  {#if errorMessage}
    <p class="snap__msg bad" data-testid="snapshot-error">{errorMessage}</p>
  {/if}

  <div class="snap__chips">
    {#each FACETS as f (f.id)}
      <button
        type="button"
        class="snap__chip"
        class:snap__chip--on={statusFacet === f.id}
        onclick={() => (statusFacet = f.id)}
      >
        {f.label}
      </button>
    {/each}
  </div>

  {#if loadState === "loading" && !data}
    <p class="snap__empty">Betöltés… / Loading…</p>
  {:else if visibleRows.length === 0}
    <p class="snap__empty" data-testid="snapshot-empty">
      Nincs pillanatkép. / No snapshots. Kattints a „Pillanatkép most” gombra. /
      Click “Snapshot now”.
    </p>
  {:else}
    <table class="snap__tbl" data-testid="snapshot-table">
      <thead>
        <tr>
          <th>
            <button type="button" class="snap__sort" onclick={() => setSort("seq")}>
              # <span aria-hidden="true">{sortIndicator("seq")}</span>
            </button>
          </th>
          <th>
            <button type="button" class="snap__sort" onclick={() => setSort("created_at")}>
              Időbélyeg (UTC) / Timestamp <span aria-hidden="true">{sortIndicator("created_at")}</span>
            </button>
          </th>
          <th class="snap__num">
            <button type="button" class="snap__sort" onclick={() => setSort("byte_size")}>
              Méret / Size <span aria-hidden="true">{sortIndicator("byte_size")}</span>
            </button>
          </th>
          <th>Állapot / Status</th>
          <th class="snap__num">Számlák / Invoices</th>
          <th class="snap__num">Napló / Audit</th>
          <th>Kor / Age</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {#each visibleRows as r (r.seq)}
          <tr class:snap__row--bad={!r.valid}>
            <td><code>#{r.seq}</code></td>
            <td><time>{r.created_at}</time></td>
            <td class="snap__num">{r.size_human}</td>
            <td>
              {#if r.valid}
                <span class="snap__status snap__status--ok">érvényes / valid</span>
              {:else}
                <span class="snap__status snap__status--bad" title={r.validation_error ?? ""}>
                  HIBÁS / INVALID
                </span>
              {/if}
            </td>
            <td class="snap__num">{r.invoice_count < 0 ? "—" : r.invoice_count}</td>
            <td class="snap__num">{r.audit_count}</td>
            <td>{r.age_human}</td>
            <td>
              <button
                type="button"
                class="snap__btn snap__btn--sm"
                onclick={() => openRestore(r.seq)}
              >
                Visszaállítás / Restore
              </button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if restoreOpen}
    <div class="snap__wizard" data-testid="restore-wizard">
      <h2>Visszaállítás / Restore snapshot</h2>
      <p class="snap__warn">
        ⚠ A visszaállítás külön útvonalra épít — soha nem írja felül az élő
        adatbázist. Utána állítsd le a szervert és cseréld be kézzel. /
        Restore builds a side path — never overwrites the live DB. Stop serve
        and swap it in afterwards.
      </p>
      <label class="snap__field">
        <span>Pillanatkép (seq vagy időbélyeg) / Snapshot (seq or timestamp)</span>
        <input type="text" bind:value={restoreSelector} placeholder="42" />
      </label>
      <label class="snap__field">
        <span>Cél útvonal / Target path</span>
        <input
          type="text"
          bind:value={restoreTarget}
          placeholder="/Users/te/recovery/aberp.duckdb"
        />
      </label>
      {#if targetWarning}
        <p class="snap__msg bad" data-testid="restore-target-warning">{targetWarning}</p>
      {/if}
      <label class="snap__check">
        <input type="checkbox" bind:checked={restoreConfirm} />
        <span>Megerősítem / I confirm this writes a DB at the target path</span>
      </label>
      <div class="snap__wizard-actions">
        <button
          type="button"
          class="snap__btn snap__btn--primary"
          onclick={submitRestore}
          disabled={!canSubmit || restoreBusy}
          data-testid="restore-submit"
        >
          {restoreBusy ? "Folyamatban… / Working…" : "Visszaállítás / Restore"}
        </button>
        <button type="button" class="snap__btn" onclick={() => (restoreOpen = false)}>
          Mégse / Cancel
        </button>
      </div>
      {#if restoreMessage}
        <p class="snap__msg ok" data-testid="restore-result">{restoreMessage}</p>
      {/if}
      {#if restoreError}
        <p class="snap__msg bad" data-testid="restore-error">{restoreError}</p>
      {/if}
    </div>
  {/if}
</section>

<style>
  .snap {
    padding: var(--space-4);
    color: var(--color-text-primary);
    font-family: var(--type-family-body);
  }
  .snap__head {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: var(--space-4);
    margin-bottom: var(--space-3);
  }
  .snap__title h1 {
    margin: 0;
    font-size: var(--type-size-xl);
    color: var(--color-text-strong);
  }
  .snap__sub {
    margin: var(--space-1) 0 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }
  .snap__actions {
    display: flex;
    gap: var(--space-2);
  }
  .snap__btn {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    cursor: pointer;
    border-radius: var(--radius-sm);
  }
  .snap__btn:hover:not(:disabled) {
    background: var(--color-surface-divider);
  }
  .snap__btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .snap__btn--primary {
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
    border-color: var(--color-signal-positive);
  }
  .snap__btn--sm {
    padding: 2px var(--space-2);
    font-size: var(--type-size-xs);
  }
  .snap__banner {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
    margin-bottom: var(--space-3);
  }
  .snap__banner .ok {
    color: var(--color-signal-positive);
  }
  .snap__banner .bad {
    color: var(--color-signal-negative);
  }
  .snap__store {
    font-family: var(--type-family-mono);
    color: var(--color-text-muted);
    max-width: 28ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .snap__msg {
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    margin: var(--space-2) 0;
  }
  .snap__msg.ok {
    color: var(--color-signal-positive);
    border-left: 3px solid var(--color-signal-positive);
    background: var(--color-surface-raised);
  }
  .snap__msg.bad {
    color: var(--color-signal-negative);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
  }
  .snap__chips {
    display: flex;
    gap: var(--space-2);
    margin-bottom: var(--space-2);
  }
  .snap__chip {
    padding: 2px var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    cursor: pointer;
    border-radius: var(--radius-lg);
  }
  .snap__chip--on {
    background: var(--color-text-strong);
    color: var(--color-surface-base);
    border-color: var(--color-text-strong);
  }
  .snap__empty {
    padding: var(--space-5);
    text-align: center;
    color: var(--color-text-muted);
  }
  .snap__tbl {
    width: 100%;
    border-collapse: collapse;
    border: 1px solid var(--color-surface-divider);
    font-size: var(--type-size-sm);
  }
  .snap__tbl th,
  .snap__tbl td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    text-align: left;
  }
  .snap__tbl thead {
    background: var(--color-surface-raised);
  }
  .snap__tbl tbody tr:hover {
    background: var(--color-surface-raised);
  }
  .snap__num {
    text-align: right;
    font-family: var(--type-family-mono);
  }
  .snap__sort {
    background: none;
    border: none;
    color: var(--color-text-strong);
    font: inherit;
    cursor: pointer;
    padding: 0;
  }
  .snap__row--bad td {
    background: color-mix(in srgb, var(--color-signal-negative) 12%, transparent);
  }
  .snap__status {
    padding: 2px var(--space-2);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-xs);
  }
  .snap__status--ok {
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
  }
  .snap__status--bad {
    background: var(--color-signal-negative);
    color: var(--color-surface-base);
  }
  .snap__wizard {
    margin-top: var(--space-4);
    padding: var(--space-4);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-sunken);
    border-radius: var(--radius-sm);
    max-width: 640px;
  }
  .snap__wizard h2 {
    margin: 0 0 var(--space-2);
    font-size: var(--type-size-lg);
    color: var(--color-text-strong);
  }
  .snap__warn {
    color: var(--color-signal-warning);
    font-size: var(--type-size-sm);
    border-left: 3px solid var(--color-signal-warning);
    padding-left: var(--space-3);
    margin: 0 0 var(--space-3);
  }
  .snap__field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-bottom: var(--space-3);
  }
  .snap__field span {
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }
  .snap__field input {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }
  .snap__check {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--type-size-sm);
    margin-bottom: var(--space-3);
  }
  .snap__wizard-actions {
    display: flex;
    gap: var(--space-2);
  }
</style>
