<script lang="ts">
  // ADR-0112 Part C (D-19 slice C3) — drilling-rate create/edit modal.
  // `rate === null` opens create mode (POST); a non-null value pre-fills for
  // an edit (PUT). Mirrors MachineRateForm.svelte: native <dialog> modal,
  // validation-envelope inline errors, dark CSS tokens.
  //
  // Feed 0 is a VALID, meaningful value — it keeps the row inert (no drilling
  // priced for that material). Setting a positive feed is what switches
  // pricing on. The two end-condition factors must be >= 1.0 (a penalty,
  // never a discount) — the backend rejects anything less, surfaced inline.

  import { createDrillingRate, updateDrillingRate, type DrillingRate } from "../lib/api";
  import {
    composeDrillingRateInputs,
    emptyDrillingRateForm,
    formFromDrillingRate,
    parseDrillingRateValidationError,
    type DrillingRateFormState,
  } from "../lib/drilling-rates";

  interface Props {
    /** `null` for create mode; a populated rate for edit mode. */
    rate: DrillingRate | null;
    /** Invoked after a successful POST or PUT (parent reloads the list). */
    onSaved: () => void;
    /** Invoked on Cancel / backdrop / ESC. */
    onClose: () => void;
  }

  let { rate, onSaved, onClose }: Props = $props();

  const isEdit = $derived(rate !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: DrillingRateFormState = $state(emptyDrillingRateForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  $effect(() => {
    if (rate !== null) {
      form = formFromDrillingRate(rate);
    }
  });

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    fieldErrors = {};
    submitting = true;
    try {
      const body = composeDrillingRateInputs(form);
      if (rate === null) {
        await createDrillingRate(body);
      } else {
        await updateDrillingRate(rate.id, body);
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseDrillingRateValidationError(message);
      if (typed !== null) {
        const next: Record<string, string> = {};
        for (const f of typed.fields) {
          next[f.field] = f.message;
        }
        fieldErrors = next;
        submitError = "Some fields need attention — see the inline messages.";
      } else {
        submitError = message;
      }
    } finally {
      submitting = false;
    }
  }

  function onCancel() {
    if (dialogEl?.open) dialogEl.close();
    onClose();
  }

  function onDialogClick(event: MouseEvent) {
    if (event.target === dialogEl) {
      dialogEl?.close();
      onClose();
    }
  }

  function onDialogClose() {
    onClose();
  }
</script>

<dialog
  bind:this={dialogEl}
  class="machine-form"
  onclose={onDialogClose}
  onclick={onDialogClick}
  aria-label={isEdit ? "Edit drilling rate" : "New drilling rate"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>{isEdit ? "Edit drilling rate" : "New drilling rate"}</h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    <fieldset disabled={submitting} class="body">
      <section class="column">
        <h3 class="section">Material &amp; feed</h3>

        <label class="field">
          <span class="field__label">
            Material group *
            <span class="field__hint">matches a material grade, e.g. Ti-6Al-4V</span>
          </span>
          <input
            type="text"
            bind:value={form.materialGroup}
            autocomplete="off"
            readonly={isEdit}
            aria-invalid={fieldErrors.material_group !== undefined}
            data-testid="drill-material-group"
          />
          {#if fieldErrors.material_group !== undefined}
            <span class="field__error">{fieldErrors.material_group}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Cutting feed
            <span class="field__hint">mm/min per mm of ⌀ — 0 = inert (no drilling priced)</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.feedMmPerMinPerMmDia}
            autocomplete="off"
            aria-invalid={fieldErrors.feed_mm_per_min_per_mm_dia !== undefined}
            data-testid="drill-feed"
          />
          {#if fieldErrors.feed_mm_per_min_per_mm_dia !== undefined}
            <span class="field__error">{fieldErrors.feed_mm_per_min_per_mm_dia}</span>
          {/if}
        </label>
      </section>

      <section class="column">
        <h3 class="section">Peck &amp; rapids</h3>

        <label class="field">
          <span class="field__label">
            Peck depth
            <span class="field__hint">× diameter — full-depth peck cycle</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.peckDepthDiaMultiple}
            autocomplete="off"
            aria-invalid={fieldErrors.peck_depth_dia_multiple !== undefined}
            data-testid="drill-peck-depth"
          />
          {#if fieldErrors.peck_depth_dia_multiple !== undefined}
            <span class="field__error">{fieldErrors.peck_depth_dia_multiple}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Peck retract
            <span class="field__hint">seconds per retract-and-return</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.peckRetractSec}
            autocomplete="off"
            aria-invalid={fieldErrors.peck_retract_sec !== undefined}
            data-testid="drill-peck-retract"
          />
          {#if fieldErrors.peck_retract_sec !== undefined}
            <span class="field__error">{fieldErrors.peck_retract_sec}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Rapid per hole
            <span class="field__hint">seconds approach + retract, once per hole</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.rapidPerHoleSec}
            autocomplete="off"
            aria-invalid={fieldErrors.rapid_per_hole_sec !== undefined}
            data-testid="drill-rapid"
          />
          {#if fieldErrors.rapid_per_hole_sec !== undefined}
            <span class="field__error">{fieldErrors.rapid_per_hole_sec}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Tool change
            <span class="field__hint">seconds, once per distinct diameter</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.toolChangeSec}
            autocomplete="off"
            aria-invalid={fieldErrors.tool_change_sec !== undefined}
            data-testid="drill-tool-change"
          />
          {#if fieldErrors.tool_change_sec !== undefined}
            <span class="field__error">{fieldErrors.tool_change_sec}</span>
          {/if}
        </label>
      </section>

      <section class="column">
        <h3 class="section">End-condition penalties</h3>

        <label class="field">
          <span class="field__label">
            Flat-bottom factor
            <span class="field__hint">&gt;= 1.0 — slower than a point drill</span>
          </span>
          <input
            type="number"
            step="any"
            min="1"
            bind:value={form.flatBottomFactor}
            autocomplete="off"
            aria-invalid={fieldErrors.flat_bottom_factor !== undefined}
            data-testid="drill-flat-bottom"
          />
          {#if fieldErrors.flat_bottom_factor !== undefined}
            <span class="field__error">{fieldErrors.flat_bottom_factor}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Unknown-end factor
            <span class="field__hint">&gt;= 1.0 — the conservative branch</span>
          </span>
          <input
            type="number"
            step="any"
            min="1"
            bind:value={form.unknownEndConditionFactor}
            autocomplete="off"
            aria-invalid={fieldErrors.unknown_end_condition_factor !== undefined}
            data-testid="drill-unknown-end"
          />
          {#if fieldErrors.unknown_end_condition_factor !== undefined}
            <span class="field__error">{fieldErrors.unknown_end_condition_factor}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">Notes</span>
          <input
            type="text"
            bind:value={form.notes}
            autocomplete="off"
            data-testid="drill-notes"
          />
        </label>
      </section>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not save rate.</strong>
          <p class="error__detail">{submitError}</p>
        </div>
      {/if}

      <div class="actions">
        <button type="button" class="quiet-button" onclick={onCancel}>Cancel</button>
        <button type="submit" class="primary" disabled={submitting}>
          {#if submitting}
            Saving…
          {:else}
            {isEdit ? "Save changes" : "Create rate"}
          {/if}
        </button>
      </div>
    </fieldset>
  </form>
</dialog>

<style>
  dialog.machine-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 520px;
    overflow: hidden;
  }

  dialog.machine-form::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    max-height: 90vh;
    overflow: auto;
    padding: var(--space-4) var(--space-5);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
  }

  h2 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .body {
    border: 0;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .column {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .section {
    margin: var(--space-3) 0 0 0;
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-strong);
    border-bottom: 1px solid var(--color-surface-divider);
    padding-bottom: var(--space-1);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .field__label {
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
    font-weight: 500;
  }

  .field__hint {
    margin-left: var(--space-2);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    font-weight: 400;
  }

  .field input {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .field input:disabled,
  .field input:read-only {
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
    cursor: not-allowed;
  }

  .field input[aria-invalid="true"] {
    border-color: var(--color-signal-negative);
  }

  .field__error {
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
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
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .primary:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    font-size: var(--type-size-sm);
  }

  .error__detail {
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }
</style>
