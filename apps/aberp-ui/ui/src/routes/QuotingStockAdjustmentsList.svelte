<script lang="ts">
  // S267 / PR-256 — Maintenance → Quoting → Stock adjustments page.
  // Operator-managed CRUD over `quoting_stock_adjustments`: per-
  // material × per-stock-status signed % price tweak. The future
  // engine reads these as the final pricing pass: `price *= 1 + pct`.

  import { onMount } from "svelte";

  import {
    createStockAdjustment,
    deleteStockAdjustment,
    listQuotingMaterials,
    listStockAdjustments,
    updateStockAdjustment,
    type QuotingMaterial,
    type StockAdjustment,
    type StockAdjustmentInput,
    type StockStatus,
  } from "../lib/api";
  import {
    stockStatusLabel,
    type SortKey,
  } from "../lib/material-catalogue";
  import { fmtPct } from "../lib/quoting-tunables-format";
  import { isDemoMode } from "../lib/workshop-demo-mode";
  import ConfirmActionModal from "../lib/ConfirmActionModal.svelte";

  type LoadState = "idle" | "loading" | "ready" | "error";

  // SortKey is imported to keep the material-catalogue shared helper
  // referenced even though this list doesn't currently surface sort UI.
  // Keeps the SPA lint clean if shared-lib reshuffles in S268+.
  void ({} as Partial<Record<SortKey, true>>);

  const STOCK_STATUSES: readonly StockStatus[] = [
    "in_stock",
    "source_1_2d",
    "source_3_7d",
    "special_order",
  ] as const;

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let rows = $state<StockAdjustment[]>([]);
  let materials = $state<QuotingMaterial[]>([]);

  let editing = $state<StockAdjustment | null>(null);
  let adding = $state<boolean>(false);
  let saving = $state(false);
  let saveError = $state<string | null>(null);

  let draft = $state<StockAdjustmentInput>(emptyDraft());

  let confirmDelete = $state<StockAdjustment | null>(null);
  let deleting = $state(false);
  let deleteError = $state<string | null>(null);

  const demo = isDemoMode();

  function emptyDraft(): StockAdjustmentInput {
    return {
      grade: "",
      stock_status: "in_stock",
      price_adjustment_pct: 0,
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
      const [adjRes, matRes] = await Promise.all([
        listStockAdjustments(),
        listQuotingMaterials(),
      ]);
      rows = adjRes.adjustments;
      materials = matRes.materials;
      if (draft.grade === "" && materials.length > 0) {
        draft.grade = materials[0].grade;
      }
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function openAdd(): void {
    if (demo) return;
    draft = emptyDraft();
    if (materials.length > 0) draft.grade = materials[0].grade;
    saveError = null;
    adding = true;
    editing = null;
  }

  function openEdit(r: StockAdjustment): void {
    if (demo) return;
    draft = {
      grade: r.grade,
      stock_status: r.stock_status,
      price_adjustment_pct: r.price_adjustment_pct,
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
        await updateStockAdjustment(editing.id, draft);
      } else {
        await createStockAdjustment(draft);
      }
      closeForm();
      await refresh();
    } catch (e) {
      saveError = e instanceof Error ? e.message : String(e);
    } finally {
      saving = false;
    }
  }

  function askDelete(r: StockAdjustment): void {
    if (demo) return;
    deleteError = null;
    confirmDelete = r;
  }

  async function doDelete(): Promise<void> {
    if (confirmDelete === null) return;
    deleting = true;
    deleteError = null;
    try {
      await deleteStockAdjustment(confirmDelete.id);
      confirmDelete = null;
      await refresh();
    } catch (e) {
      deleteError = e instanceof Error ? e.message : String(e);
    } finally {
      deleting = false;
    }
  }
</script>

<section class="qt-page" data-testid="stock-adjustments-section">
  <header class="qt-page__head">
    <div>
      <h2 class="qt-page__title">
        Készlet-korrekciók / Stock adjustments
        <span class="qt-page__hint">
          Anyag × készletállapot szerinti előjeles ±% ártűzés /
          Per-material × stock-status signed % price tweak
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
      <button
        type="button"
        class="qt-page__add"
        disabled={demo || materials.length === 0}
        onclick={openAdd}
      >
        + Korrekció / Adjustment
      </button>
    </div>
  </header>

  {#if demo}
    <div class="qt-page__demo" role="status">
      Demo mód — módosítás letiltva. / Demo mode — changes disabled.
    </div>
  {/if}

  {#if materials.length === 0}
    <div class="qt-page__notice" role="status">
      Először vegyél fel anyagot az Anyagkatalógusban. /
      Add a material to the catalogue first.
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
      <p>Még nincs készlet-korrekció. / No stock adjustments yet.</p>
    </div>
  {:else}
    <table class="qt-table">
      <thead>
        <tr>
          <th>Anyag / Grade</th>
          <th>Készlet / Stock</th>
          <th>Ár-tűzés / Adjustment</th>
          <th>Jegyzet / Notes</th>
          <th>Módosítva / Updated</th>
          <th>Művelet</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as r (r.id)}
          <tr data-testid="stock-adj-row">
            <td class="qt-table__grade">{r.grade}</td>
            <td>{stockStatusLabel(r.stock_status)}</td>
            <td class="num" data-pct-sign={r.price_adjustment_pct < 0 ? "neg" : "pos"}>
              {fmtPct(r.price_adjustment_pct)}
            </td>
            <td>{r.notes ?? ""}</td>
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
  <dialog open class="qt-form" data-testid="stock-adj-form">
    <h3>
      {editing !== null ? "Korrekció szerkesztése / Edit adjustment" : "Új korrekció / New adjustment"}
    </h3>
    <label>
      <span>Anyag / Material grade</span>
      <select bind:value={draft.grade}>
        {#each materials as m (m.grade)}
          <option value={m.grade}>{m.grade} — {m.display_name}</option>
        {/each}
      </select>
    </label>
    <label>
      <span>Készlet-állapot / Stock status</span>
      <select bind:value={draft.stock_status}>
        {#each STOCK_STATUSES as s}
          <option value={s}>{stockStatusLabel(s)}</option>
        {/each}
      </select>
    </label>
    <label>
      <span>Ár-tűzés (törtszám, pl. -0.05 = −5%) / Adjustment (fraction)</span>
      <input
        type="number"
        step="0.01"
        min="-1"
        max="1"
        bind:value={draft.price_adjustment_pct}
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
    title="Korrekció törlése? / Delete adjustment?"
    body={`${confirmDelete.grade} · ${stockStatusLabel(confirmDelete.stock_status)} · ${fmtPct(confirmDelete.price_adjustment_pct)}`}
    consequence="A jövőbeli árajánlatok újra alapáron mennek. / Future quotes revert to base pricing."
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
  .qt-page__notice {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
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
  }
  .qt-table td.num[data-pct-sign="neg"] {
    color: var(--color-signal-positive);
  }
  .qt-table td.num[data-pct-sign="pos"] {
    color: var(--color-signal-warning);
  }
  .qt-table__grade {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
    font-weight: 600;
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
