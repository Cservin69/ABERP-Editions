<script lang="ts">
  // S6 / ADR-0094 Gap 3 — gear-process create/edit modal. `rate === null`
  // opens create mode (POST); a non-null value pre-fills for an edit (PUT).
  // Mirrors MachineRateForm.svelte: native <dialog> modal, A157 validation-
  // envelope inline errors, dark CSS tokens.

  import {
    createGearProcess,
    updateGearProcess,
    type GearProcessRate,
  } from "../lib/api";
  import {
    GEAR_PROCESSES,
    composeGearProcessInputs,
    emptyGearProcessForm,
    formFromGearProcess,
    parseGearProcessValidationError,
    type GearProcessFormState,
  } from "../lib/gear-processes";

  interface Props {
    /** `null` for create mode; a populated rate for edit mode. */
    rate: GearProcessRate | null;
    /** Invoked after a successful POST or PUT (parent reloads the list). */
    onSaved: () => void;
    /** Invoked on Cancel / backdrop / ESC. */
    onClose: () => void;
  }

  let { rate, onSaved, onClose }: Props = $props();

  const isEdit = $derived(rate !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: GearProcessFormState = $state(emptyGearProcessForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  $effect(() => {
    if (rate !== null) {
      form = formFromGearProcess(rate);
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
      const body = composeGearProcessInputs(form);
      if (rate === null) {
        await createGearProcess(body);
      } else {
        await updateGearProcess(rate.id, body);
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseGearProcessValidationError(message);
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
  aria-label={isEdit ? "Edit gear process" : "New gear process"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>{isEdit ? "Edit gear process" : "New gear process"}</h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    <fieldset disabled={submitting} class="body">
      <section class="column">
        <h3 class="section">Process &amp; coefficients</h3>

        <label class="field">
          <span class="field__label">Gear process *</span>
          <select bind:value={form.process} data-testid="gear-process">
            {#each GEAR_PROCESSES as proc (proc.value)}
              <option value={proc.value}>{proc.label}</option>
            {/each}
          </select>
          {#if fieldErrors.process !== undefined}
            <span class="field__error">{fieldErrors.process}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Setup
            <span class="field__hint">minutes / gear (indexing, tool-load)</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.setupMin}
            autocomplete="off"
            aria-invalid={fieldErrors.setup_min !== undefined}
            data-testid="gear-setup"
          />
          {#if fieldErrors.setup_min !== undefined}
            <span class="field__error">{fieldErrors.setup_min}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Per tooth
            <span class="field__hint">minutes / generated tooth</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.minPerTooth}
            autocomplete="off"
            aria-invalid={fieldErrors.min_per_tooth !== undefined}
            data-testid="gear-mpt"
          />
          {#if fieldErrors.min_per_tooth !== undefined}
            <span class="field__error">{fieldErrors.min_per_tooth}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Module exponent
            <span class="field__hint">time ∝ module^this</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.moduleExponent}
            autocomplete="off"
            aria-invalid={fieldErrors.module_exponent !== undefined}
            data-testid="gear-mexp"
          />
          {#if fieldErrors.module_exponent !== undefined}
            <span class="field__error">{fieldErrors.module_exponent}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            AGMA quality factor base
            <span class="field__hint">+per class above datum 8</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.agmaQualityFactorBase}
            autocomplete="off"
            aria-invalid={fieldErrors.agma_quality_factor_base !== undefined}
            data-testid="gear-agma"
          />
          {#if fieldErrors.agma_quality_factor_base !== undefined}
            <span class="field__error">{fieldErrors.agma_quality_factor_base}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            In-cycle factor
            <span class="field__hint">(0, 1] — &lt;1 only for in-cycle skive</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            max="1"
            bind:value={form.inCycleFactor}
            autocomplete="off"
            aria-invalid={fieldErrors.in_cycle_factor !== undefined}
            data-testid="gear-incycle"
          />
          {#if fieldErrors.in_cycle_factor !== undefined}
            <span class="field__error">{fieldErrors.in_cycle_factor}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">Notes</span>
          <input
            type="text"
            bind:value={form.notes}
            autocomplete="off"
            data-testid="gear-notes"
          />
        </label>
      </section>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not save process.</strong>
          <p class="error__detail">{submitError}</p>
        </div>
      {/if}

      <div class="actions">
        <button type="button" class="quiet-button" onclick={onCancel}>Cancel</button>
        <button type="submit" class="primary" disabled={submitting}>
          {#if submitting}
            Saving…
          {:else}
            {isEdit ? "Save changes" : "Create process"}
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

  .field input,
  .field select {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .field input:disabled {
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
