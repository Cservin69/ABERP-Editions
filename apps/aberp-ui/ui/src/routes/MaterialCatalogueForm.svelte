<script lang="ts">
  // S266 / PR-255 — Add / Edit material grade modal. Mirrors
  // AdapterForm.svelte's <dialog> frame; token-styled per
  // [[spa-dark-theme-default]] (every colour resolves to a tokens.css
  // variable). On Add the `grade` (PRIMARY KEY) is editable; on Edit it is
  // fixed (changing the key is a delete-then-add). The backend is the
  // authority on the numeric invariants — the client checks below are for
  // immediate feedback only.

  import { untrack } from "svelte";
  import {
    createQuotingMaterial,
    updateQuotingMaterial,
    type QuotingMaterial,
    type StockStatus,
  } from "../lib/api";
  import { STOCK_STATUS_ORDER, stockStatusLabel } from "../lib/material-catalogue";

  interface Props {
    mode: "add" | "edit";
    initial?: QuotingMaterial | null;
    onSaved: () => void;
    onCancel: () => void;
  }

  let { mode, initial = null, onCancel, onSaved }: Props = $props();

  // One-shot snapshot of the seed (the parent re-mounts per open).
  const seed = untrack(() => initial);

  let grade = $state(seed?.grade ?? "");
  let displayName = $state(seed?.display_name ?? "");
  let density = $state<number | null>(seed?.density_g_cm3 ?? null);
  let cost = $state<number | null>(seed?.cost_per_kg_eur ?? null);
  let machiningDifficulty = $state<number>(seed?.machining_difficulty ?? 1.0);
  let carbide = $state<number>(seed?.carbide_life_multiplier ?? 1.0);
  let stockStatus = $state<StockStatus>(seed?.stock_status ?? "in_stock");
  let leadDays = $state<number | null>(seed?.lead_time_default_days ?? 0);
  let quoteMultiplier = $state<number>(seed?.quote_multiplier ?? 1.0);
  let notes = $state(seed?.notes ?? "");

  let busy = $state(false);
  let error = $state<string | null>(null);

  let dialogEl = $state<HTMLDialogElement | null>(null);

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  function handleCancel(): void {
    if (busy) return;
    if (dialogEl?.open) dialogEl.close();
    onCancel();
  }

  function onDialogClick(event: MouseEvent): void {
    if (event.target === dialogEl) handleCancel();
  }

  /** Light client-side guard mirroring the backend invariants — the
   * server still validates loud, this is just for immediate feedback. */
  function clientError(): string | null {
    if (grade.trim().length === 0) return "A grade kötelező / Grade is required.";
    if (displayName.trim().length === 0)
      return "A megjelenítendő név kötelező / Display name is required.";
    const d = Number(density);
    if (!Number.isFinite(d) || d <= 0)
      return "A sűrűség > 0 legyen / Density must be > 0.";
    const c = Number(cost);
    if (!Number.isFinite(c) || c < 0)
      return "A költség >= 0 legyen / Cost must be >= 0.";
    const l = Number(leadDays);
    if (!Number.isInteger(l) || l < 0)
      return "A szállítási idő nem-negatív egész / Lead time must be a non-negative integer.";
    for (const [name, v] of [
      ["megmunkálási nehézség / machining difficulty", machiningDifficulty],
      ["karbid-szorzó / carbide multiplier", carbide],
      ["ár-szorzó / quote multiplier", quoteMultiplier],
    ] as const) {
      if (!Number.isFinite(Number(v)) || Number(v) <= 0)
        return `${name} > 0 legyen / must be > 0.`;
    }
    return null;
  }

  async function handleSave(): Promise<void> {
    if (busy) return;
    const ce = clientError();
    if (ce !== null) {
      error = ce;
      return;
    }
    error = null;
    busy = true;
    const body = {
      grade: grade.trim(),
      display_name: displayName.trim(),
      density_g_cm3: Number(density),
      cost_per_kg_eur: Number(cost),
      machining_difficulty: Number(machiningDifficulty),
      carbide_life_multiplier: Number(carbide),
      stock_status: stockStatus,
      lead_time_default_days: Number(leadDays),
      quote_multiplier: Number(quoteMultiplier),
      notes: notes.trim().length > 0 ? notes.trim() : null,
    };
    try {
      if (mode === "add") {
        await createQuotingMaterial(body);
      } else {
        await updateQuotingMaterial(grade.trim(), body);
      }
      if (dialogEl?.open) dialogEl.close();
      onSaved();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="material-form"
  onclick={onDialogClick}
  aria-label={mode === "add" ? "Add material" : "Edit material"}
  data-testid="material-form"
>
  <div class="frame">
    <header class="head">
      <h2>
        {mode === "add"
          ? "Anyag hozzáadása / Add material"
          : "Anyag szerkesztése / Edit material"}
      </h2>
    </header>

    {#if error !== null}
      <p class="form-error" role="alert" data-testid="material-form-error">
        {error}
      </p>
    {/if}

    <div class="fields">
      <div class="field-row">
        <label class="field field--grow">
          <span class="field__label">Anyagminőség / Grade</span>
          <input
            type="text"
            bind:value={grade}
            disabled={mode === "edit" || busy}
            placeholder="6061-T6"
            data-testid="material-form-grade"
          />
          {#if mode === "edit"}
            <span class="field__hint">
              A kulcs nem módosítható / The key is immutable
            </span>
          {/if}
        </label>
        <label class="field field--grow">
          <span class="field__label">Megnevezés / Display name</span>
          <input
            type="text"
            bind:value={displayName}
            disabled={busy}
            placeholder="Aluminium 6061-T6"
            data-testid="material-form-display-name"
          />
        </label>
      </div>

      <div class="field-row">
        <label class="field field--grow">
          <span class="field__label">Sűrűség g/cm³ / Density</span>
          <input
            type="number"
            step="0.01"
            bind:value={density}
            disabled={busy}
            data-testid="material-form-density"
          />
        </label>
        <label class="field field--grow">
          <span class="field__label">Költség €/kg / Cost</span>
          <input
            type="number"
            step="0.01"
            bind:value={cost}
            disabled={busy}
            data-testid="material-form-cost"
          />
        </label>
      </div>

      <div class="field-row">
        <label class="field field--grow">
          <span class="field__label">Megmunkálási nehézség / Machining difficulty</span>
          <input
            type="number"
            step="0.1"
            bind:value={machiningDifficulty}
            disabled={busy}
            data-testid="material-form-machining-difficulty"
          />
          <span class="field__hint">1.0 = 6061-T6 referencia; nagyobb = lassabb / harder = slower</span>
        </label>
        <label class="field field--grow">
          <span class="field__label">Karbid-szorzó / Carbide life mult.</span>
          <input
            type="number"
            step="0.1"
            bind:value={carbide}
            disabled={busy}
            data-testid="material-form-carbide"
          />
        </label>
      </div>

      <div class="field-row">
        <label class="field field--grow">
          <span class="field__label">Készlet-állapot / Stock status</span>
          <select
            bind:value={stockStatus}
            disabled={busy}
            data-testid="material-form-stock-status"
          >
            {#each STOCK_STATUS_ORDER as s (s)}
              <option value={s}>{stockStatusLabel(s)}</option>
            {/each}
          </select>
        </label>
        <label class="field field--grow">
          <span class="field__label">Szállítási idő (nap) / Lead time (days)</span>
          <input
            type="number"
            min="0"
            step="1"
            bind:value={leadDays}
            disabled={busy}
            data-testid="material-form-lead-days"
          />
        </label>
      </div>

      <div class="field-row">
        <label class="field field--grow">
          <span class="field__label">Ár-szorzó / Quote multiplier</span>
          <input
            type="number"
            step="0.05"
            bind:value={quoteMultiplier}
            disabled={busy}
            data-testid="material-form-quote-multiplier"
          />
          <span class="field__hint">Operátor felülbírálat / operator override (default 1.0)</span>
        </label>
      </div>

      <label class="field">
        <span class="field__label">Megjegyzés / Notes</span>
        <textarea
          rows="2"
          bind:value={notes}
          disabled={busy}
          data-testid="material-form-notes"
        ></textarea>
      </label>
    </div>

    <footer class="actions">
      <button
        type="button"
        class="quiet-button"
        onclick={handleCancel}
        disabled={busy}
      >
        Mégse / Cancel
      </button>
      <button
        type="button"
        class="primary"
        onclick={() => void handleSave()}
        disabled={busy}
        aria-busy={busy}
        data-testid="material-form-save"
      >
        {busy ? "Mentés…" : "Mentés / Save"}
      </button>
    </footer>
  </div>
</dialog>

<style>
  dialog.material-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    width: 620px;
    overflow: hidden;
  }

  dialog.material-form::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-4) var(--space-5);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .head h2 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .form-error {
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-signal-negative);
    background: var(--color-surface-sunken);
    color: var(--color-signal-negative);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
  }

  .fields {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .field-row {
    display: flex;
    gap: var(--space-3);
  }

  .field--grow {
    flex: 1 1 auto;
  }

  .field__label {
    font-size: var(--type-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-text-muted);
  }

  .field__hint {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .field input,
  .field select,
  .field textarea {
    padding: var(--space-2);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
    border-radius: var(--radius-sm);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }

  .field input:focus,
  .field select:focus,
  .field textarea:focus {
    outline: none;
    border-color: var(--color-signal-positive);
  }

  .field input:disabled,
  .field select:disabled,
  .field textarea:disabled {
    opacity: 0.6;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2) var(--space-4);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    border-radius: var(--radius-sm);
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .primary {
    padding: var(--space-2) var(--space-5);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-signal-positive);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .primary:hover:not(:disabled) {
    color: var(--color-signal-positive);
  }

  .primary:disabled,
  .quiet-button:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
