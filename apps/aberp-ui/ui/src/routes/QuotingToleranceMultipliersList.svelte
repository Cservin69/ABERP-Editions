<script lang="ts">
  // S267 / PR-256 — Maintenance → Quoting → Tolerance multipliers
  // page. Edit-in-place over a fixed 5-row closed-vocab table. No
  // create/delete: the bands are exhaustive and seeded at boot.

  import { onMount } from "svelte";

  import {
    listToleranceMultipliers,
    updateToleranceMultiplier,
    type ToleranceMultiplier,
    type ToleranceRange,
  } from "../lib/api";
  import { toleranceRangeLabel } from "../lib/quoting-tunables-format";
  import { isDemoMode } from "../lib/workshop-demo-mode";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let rows = $state<ToleranceMultiplier[]>([]);

  let editing = $state<ToleranceMultiplier | null>(null);
  let draftMultiplier = $state<number>(1);
  let draftInsp = $state<number>(0);
  let draftNotes = $state<string>("");
  let saveError = $state<string | null>(null);
  let saving = $state(false);

  const demo = isDemoMode();

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      const res = await listToleranceMultipliers();
      rows = res.multipliers;
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function openEdit(r: ToleranceMultiplier): void {
    if (demo) return;
    editing = r;
    draftMultiplier = r.multiplier;
    draftInsp = r.inspection_minutes_per_feature;
    draftNotes = r.notes ?? "";
    saveError = null;
  }

  function closeForm(): void {
    editing = null;
    saveError = null;
  }

  async function save(): Promise<void> {
    if (editing === null) return;
    saving = true;
    saveError = null;
    try {
      await updateToleranceMultiplier(editing.tolerance_range as ToleranceRange, {
        tolerance_range: editing.tolerance_range as ToleranceRange,
        multiplier: draftMultiplier,
        inspection_minutes_per_feature: draftInsp,
        notes: draftNotes.trim() === "" ? null : draftNotes,
      });
      closeForm();
      await refresh();
    } catch (e) {
      saveError = e instanceof Error ? e.message : String(e);
    } finally {
      saving = false;
    }
  }
</script>

<section class="qt-page" data-testid="tolerance-multipliers-section">
  <header class="qt-page__head">
    <div>
      <h2 class="qt-page__title">
        Tűrés-szorzók / Tolerance multipliers
        <span class="qt-page__hint">
          Tűréstartomány szerinti megmunkálás-szorzó + ellenőrzési idő /
          Per-band machining multiplier + per-feature inspection minutes
        </span>
      </h2>
    </div>
    <div class="qt-page__actions">
      <button
        type="button"
        class="qt-page__refresh"
        disabled={loadState === "loading"}
        onclick={() => void refresh()}
      >
        {loadState === "loading" ? "Frissítés…" : "Frissítés / Refresh"}
      </button>
    </div>
  </header>

  {#if demo}
    <div class="qt-page__demo" role="status">
      Demo mód — módosítás letiltva. / Demo mode — changes disabled.
    </div>
  {/if}

  {#if loadState === "loading" && rows.length === 0}
    <p class="qt-page__muted">Betöltés… / Loading…</p>
  {:else if loadState === "error"}
    <div class="qt-page__error" role="alert">
      <strong>Sikertelen lekérdezés / Failed to load.</strong>
      <p>{errorMessage}</p>
    </div>
  {:else}
    <table class="qt-table">
      <thead>
        <tr>
          <th>Tartomány / Range</th>
          <th>Szorzó / Multiplier</th>
          <th>Ellenőrzés perc/db / Inspection min/feature</th>
          <th>Jegyzet / Notes</th>
          <th>Módosítva / Updated</th>
          <th>Művelet</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as r (r.tolerance_range)}
          <tr data-testid="tolerance-row">
            <td>{toleranceRangeLabel(r.tolerance_range)}</td>
            <td class="num">{r.multiplier.toFixed(2)}</td>
            <td class="num">{r.inspection_minutes_per_feature.toFixed(2)}</td>
            <td>{r.notes ?? ""}</td>
            <td class="qt-table__updated" title={r.updated_by_actor}>
              {r.updated_at}
            </td>
            <td>
              <button
                type="button"
                class="qt-row__edit"
                disabled={demo}
                onclick={() => openEdit(r)}
              >
                Szerkesztés / Edit
              </button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if editing !== null}
  <dialog open class="qt-form" data-testid="tolerance-form">
    <h3>
      {toleranceRangeLabel(editing.tolerance_range)}
    </h3>
    <label>
      <span>Szorzó / Multiplier</span>
      <input type="number" step="0.01" min="0.01" bind:value={draftMultiplier} />
    </label>
    <label>
      <span>Ellenőrzés perc / jellemző / Inspection min / feature</span>
      <input type="number" step="0.01" min="0" bind:value={draftInsp} />
    </label>
    <label>
      <span>Jegyzet / Notes</span>
      <input type="text" bind:value={draftNotes} />
    </label>

    {#if saveError !== null}
      <div class="qt-page__error" role="alert">
        <strong>Mentés sikertelen / Save failed.</strong>
        <p>{saveError}</p>
      </div>
    {/if}

    <div class="qt-form__actions">
      <button type="button" class="qt-form__cancel" onclick={closeForm}>
        Mégse / Cancel
      </button>
      <button
        type="button"
        class="qt-form__save"
        disabled={saving}
        onclick={() => void save()}
      >
        {saving ? "Mentés…" : "Mentés / Save"}
      </button>
    </div>
  </dialog>
{/if}

<style>
  .qt-page {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) 0;
  }
  .qt-page__head {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .qt-page__title {
    font-size: var(--type-size-lg);
    font-weight: 600;
    margin: 0;
    color: var(--color-text-strong);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .qt-page__hint {
    font-size: var(--type-size-sm);
    font-weight: 400;
    color: var(--color-text-muted);
  }
  .qt-page__actions {
    display: flex;
    gap: var(--space-2);
  }
  .qt-page__refresh,
  .qt-form__cancel,
  .qt-form__save {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }
  .qt-form__save {
    color: var(--color-text-strong);
    border-color: var(--color-signal-positive);
  }
  .qt-page__demo {
    padding: var(--space-2) var(--space-3);
    border: 1px dashed var(--color-signal-warning);
    color: var(--color-signal-warning);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
  }
  .qt-page__muted {
    color: var(--color-text-muted);
    font-style: italic;
  }
  .qt-page__error {
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-negative);
    border-radius: var(--radius-sm);
    color: var(--color-text-primary);
  }
  .qt-page__error strong {
    color: var(--color-signal-negative);
  }
  .qt-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
    background: var(--color-surface-sunken);
  }
  .qt-table th,
  .qt-table td {
    padding: var(--space-2) var(--space-3);
    text-align: left;
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
  }
  .qt-table th {
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .qt-table td.num {
    text-align: right;
    font-family: var(--type-family-mono);
    color: var(--color-text-secondary);
  }
  .qt-table__updated {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }
  .qt-row__edit {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--type-size-sm);
  }
  .qt-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    padding: var(--space-4);
    max-width: 540px;
    border-radius: var(--radius-sm);
  }
  .qt-form h3 {
    margin: 0 0 var(--space-3);
    color: var(--color-text-strong);
  }
  .qt-form label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-bottom: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .qt-form input {
    padding: var(--space-2);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }
  .qt-form__actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }
</style>
