<script lang="ts">
  // S267 / PR-256 — Maintenance → Quoting → Complexity rules page.
  // Operator-managed CRUD over `quoting_complexity_rules`: per-
  // feature × size_bucket × count-range weighting that the future
  // `aberp-quote-engine` (S268+) consumes. Dark-theme tokens
  // ([[spa-dark-theme-default]]); shape modelled on
  // MaterialCatalogueList.svelte (S266).

  import { onMount } from "svelte";

  import {
    createComplexityRule,
    deleteComplexityRule,
    listComplexityRules,
    updateComplexityRule,
    FEATURE_TYPES,
    SIZE_BUCKETS,
    type ComplexityRule,
    type ComplexityRuleInput,
    type FeatureType,
    type SizeBucket,
  } from "../lib/api";
  import {
    featureTypeLabel,
    sizeBucketLabel,
  } from "../lib/quoting-tunables-format";
  import { isDemoMode } from "../lib/workshop-demo-mode";
  import ConfirmActionModal from "../lib/ConfirmActionModal.svelte";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let rows = $state<ComplexityRule[]>([]);

  let editing = $state<ComplexityRule | null>(null);
  let adding = $state<boolean>(false);
  let saveError = $state<string | null>(null);
  let saving = $state(false);

  let confirmDelete = $state<ComplexityRule | null>(null);
  let deleting = $state(false);
  let deleteError = $state<string | null>(null);

  const demo = isDemoMode();

  let draft = $state<ComplexityRuleInput>(emptyDraft());

  function emptyDraft(): ComplexityRuleInput {
    return {
      feature_type: "hole" as FeatureType,
      size_bucket: "M" as SizeBucket,
      count_min: 1,
      count_max: null,
      base_time_minutes: 1.0,
      multiplier: 1.0,
      setup_penalty_minutes: 0.0,
      notes: null,
    };
  }

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      const res = await listComplexityRules();
      rows = res.rules;
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function openAdd(): void {
    if (demo) return;
    draft = emptyDraft();
    saveError = null;
    adding = true;
    editing = null;
  }

  function openEdit(r: ComplexityRule): void {
    if (demo) return;
    draft = {
      feature_type: r.feature_type,
      size_bucket: r.size_bucket,
      count_min: r.count_min,
      count_max: r.count_max,
      base_time_minutes: r.base_time_minutes,
      multiplier: r.multiplier,
      setup_penalty_minutes: r.setup_penalty_minutes,
      notes: r.notes,
    };
    saveError = null;
    editing = r;
    adding = false;
  }

  function closeForm(): void {
    editing = null;
    adding = false;
    saveError = null;
  }

  async function save(): Promise<void> {
    saving = true;
    saveError = null;
    try {
      if (editing !== null) {
        await updateComplexityRule(editing.id, draft);
      } else {
        await createComplexityRule(draft);
      }
      closeForm();
      await refresh();
    } catch (e) {
      saveError = e instanceof Error ? e.message : String(e);
    } finally {
      saving = false;
    }
  }

  function askDelete(r: ComplexityRule): void {
    if (demo) return;
    deleteError = null;
    confirmDelete = r;
  }

  async function doDelete(): Promise<void> {
    if (confirmDelete === null) return;
    deleting = true;
    deleteError = null;
    try {
      await deleteComplexityRule(confirmDelete.id);
      confirmDelete = null;
      await refresh();
    } catch (e) {
      deleteError = e instanceof Error ? e.message : String(e);
    } finally {
      deleting = false;
    }
  }
</script>

<section class="qt-page" data-testid="complexity-rules-section">
  <header class="qt-page__head">
    <div>
      <h2 class="qt-page__title">
        Komplexitás szabályok / Complexity rules
        <span class="qt-page__hint">
          Jellemző × méret × darab szabályok az automatikus árajánlat
          motorhoz / Feature × size × count rules for the auto-quoting engine
        </span>
      </h2>
    </div>
    <div class="qt-page__actions">
      <button
        type="button"
        class="qt-page__refresh"
        disabled={loadState === "loading"}
        onclick={() => void refresh()}
        data-testid="complexity-refresh"
      >
        {loadState === "loading" ? "Frissítés…" : "Frissítés / Refresh"}
      </button>
      <button
        type="button"
        class="qt-page__add"
        disabled={demo}
        onclick={openAdd}
        data-testid="complexity-add-btn"
      >
        + Szabály / Rule
      </button>
    </div>
  </header>

  {#if demo}
    <div class="qt-page__demo" role="status">
      Demo mód — módosítás letiltva. / Demo mode — changes disabled.
    </div>
  {/if}

  {#if deleteError !== null}
    <div class="qt-page__error" role="alert">
      <strong>Törlés sikertelen / Delete failed.</strong>
      <p>{deleteError}</p>
    </div>
  {/if}

  {#if loadState === "loading" && rows.length === 0}
    <p class="qt-page__muted">Betöltés… / Loading…</p>
  {:else if loadState === "error"}
    <div class="qt-page__error" role="alert">
      <strong>Sikertelen lekérdezés / Failed to load.</strong>
      <p>{errorMessage}</p>
    </div>
  {:else if rows.length === 0}
    <div class="qt-page__empty">
      <p>Még nincs szabály. Adj hozzá egyet a „+ Szabály" gombbal.</p>
      <p>No rules yet. Add one with "+ Rule".</p>
    </div>
  {:else}
    <table class="qt-table">
      <thead>
        <tr>
          <th>Jellemző / Feature</th>
          <th>Méret / Size</th>
          <th>Db min / count_min</th>
          <th>Db max / count_max</th>
          <th>Alap perc / base min</th>
          <th>Szorzó / Mult.</th>
          <th>Beállítás perc / Setup min</th>
          <th>Módosítva / Updated</th>
          <th>Művelet / Action</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as r (r.id)}
          <tr data-testid="complexity-row">
            <td>{featureTypeLabel(r.feature_type)}</td>
            <td>{sizeBucketLabel(r.size_bucket)}</td>
            <td class="num">{r.count_min}</td>
            <td class="num">{r.count_max ?? "∞"}</td>
            <td class="num">{r.base_time_minutes.toFixed(2)}</td>
            <td class="num">{r.multiplier.toFixed(2)}</td>
            <td class="num">{r.setup_penalty_minutes.toFixed(2)}</td>
            <td class="qt-table__updated" title={r.updated_by_actor}>
              {r.updated_at}
            </td>
            <td>
              <div class="qt-row__actions">
                <button
                  type="button"
                  class="qt-row__edit"
                  disabled={demo}
                  onclick={() => openEdit(r)}
                >
                  Szerkesztés / Edit
                </button>
                <button
                  type="button"
                  class="qt-row__delete"
                  disabled={demo}
                  onclick={() => askDelete(r)}
                >
                  Törlés / Delete
                </button>
              </div>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if adding || editing !== null}
  <dialog open class="qt-form" data-testid="complexity-form">
    <h3>
      {editing !== null ? "Szabály szerkesztése / Edit rule" : "Új szabály / New rule"}
    </h3>
    <label>
      <span>Jellemző / Feature type</span>
      <select bind:value={draft.feature_type}>
        {#each FEATURE_TYPES as t}
          <option value={t}>{featureTypeLabel(t)}</option>
        {/each}
      </select>
    </label>
    <label>
      <span>Méret tartomány / Size bucket</span>
      <select bind:value={draft.size_bucket}>
        {#each SIZE_BUCKETS as b}
          <option value={b}>{sizeBucketLabel(b)}</option>
        {/each}
      </select>
    </label>
    <label>
      <span>Darab min / count_min</span>
      <input type="number" min="0" bind:value={draft.count_min} />
    </label>
    <label>
      <span>Darab max / count_max (üres = ∞)</span>
      <input
        type="number"
        min="1"
        value={draft.count_max ?? ""}
        oninput={(e) => {
          const v = (e.target as HTMLInputElement).value;
          draft.count_max = v === "" ? null : Number(v);
        }}
      />
    </label>
    <label>
      <span>Alapidő (perc) / base_time_minutes</span>
      <input type="number" step="0.01" min="0" bind:value={draft.base_time_minutes} />
    </label>
    <label>
      <span>Szorzó / multiplier</span>
      <input type="number" step="0.01" min="0.01" bind:value={draft.multiplier} />
    </label>
    <label>
      <span>Beállítási büntetés (perc) / setup_penalty_minutes</span>
      <input
        type="number"
        step="0.01"
        min="0"
        bind:value={draft.setup_penalty_minutes}
      />
    </label>
    <label>
      <span>Jegyzet / Notes</span>
      <input
        type="text"
        value={draft.notes ?? ""}
        oninput={(e) => {
          const v = (e.target as HTMLInputElement).value;
          draft.notes = v.trim() === "" ? null : v;
        }}
      />
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

{#if confirmDelete !== null}
  <ConfirmActionModal
    title="Szabály törlése? / Delete rule?"
    body={`${featureTypeLabel(confirmDelete.feature_type)} · ${sizeBucketLabel(confirmDelete.size_bucket)} · count_min=${confirmDelete.count_min}`}
    consequence="A jövőbeli árajánlatokra hatással lehet. / Future quotes may be affected."
    confirmLabel="Törlés / Delete"
    cancelLabel="Mégse / Cancel"
    busy={deleting}
    onConfirm={() => void doDelete()}
    onCancel={() => (confirmDelete = null)}
  />
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
  .qt-page__add,
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
  .qt-page__add,
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
  .qt-page__empty {
    padding: var(--space-4);
    background: var(--color-surface-raised);
    border: 1px dashed var(--color-surface-divider);
    border-radius: var(--radius-sm);
    color: var(--color-text-secondary);
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
  .qt-row__actions {
    display: flex;
    gap: var(--space-2);
  }
  .qt-row__edit,
  .qt-row__delete {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--type-size-sm);
  }
  .qt-row__delete:hover:not(:disabled) {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
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
  .qt-form input,
  .qt-form select {
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
