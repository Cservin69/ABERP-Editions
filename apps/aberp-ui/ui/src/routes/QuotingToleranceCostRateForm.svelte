<script lang="ts">
  // T5 / ADR-0097 Part 2 — tolerance cost-rate create/edit modal. `rate ===
  // null` opens create mode (POST); a non-null value pre-fills for an edit
  // (PUT). Mirrors MachineRateForm.svelte: native <dialog> modal, A157
  // validation-envelope inline errors, dark CSS tokens. One row per band; a
  // create that collides with an existing band surfaces the backend Conflict.

  import {
    createToleranceCostRate,
    updateToleranceCostRate,
    type ToleranceCostRate,
  } from "../lib/api";
  import {
    composeToleranceCostRateInputs,
    emptyToleranceCostRateForm,
    formFromToleranceCostRate,
    parseToleranceCostRateValidationError,
    TOLERANCE_BANDS,
    type ToleranceCostRateFormState,
  } from "../lib/tolerance-cost-rates";

  interface Props {
    /** `null` for create mode; a populated rate for edit mode. */
    rate: ToleranceCostRate | null;
    /** Invoked after a successful POST or PUT (parent reloads the list). */
    onSaved: () => void;
    /** Invoked on Cancel / backdrop / ESC. */
    onClose: () => void;
  }

  let { rate, onSaved, onClose }: Props = $props();

  const isEdit = $derived(rate !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: ToleranceCostRateFormState = $state(emptyToleranceCostRateForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  $effect(() => {
    if (rate !== null) {
      form = formFromToleranceCostRate(rate);
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
      const body = composeToleranceCostRateInputs(form);
      if (rate === null) {
        await createToleranceCostRate(body);
      } else {
        await updateToleranceCostRate(rate.id, body);
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseToleranceCostRateValidationError(message);
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
  class="tol-form"
  onclose={onDialogClose}
  onclick={onDialogClick}
  aria-label={isEdit ? "Edit tolerance cost rate" : "New tolerance cost rate"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>{isEdit ? "Edit tolerance cost rate" : "New tolerance cost rate"}</h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    <fieldset disabled={submitting} class="body">
      <section class="column">
        <h3 class="section">Band &amp; cost drivers</h3>

        <label class="field">
          <span class="field__label">Tolerance band *</span>
          <select bind:value={form.toleranceClass} data-testid="rate-band">
            {#each TOLERANCE_BANDS as band (band.value)}
              <option value={band.value}>{band.label}</option>
            {/each}
          </select>
          {#if fieldErrors.tolerance_class !== undefined}
            <span class="field__error">{fieldErrors.tolerance_class}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Extra finishing passes
            <span class="field__hint">whole-part passes added</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.finishPassesAdd}
            autocomplete="off"
            aria-invalid={fieldErrors.finish_passes_add !== undefined}
            data-testid="rate-finish-passes"
          />
          {#if fieldErrors.finish_passes_add !== undefined}
            <span class="field__error">{fieldErrors.finish_passes_add}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            In-process gauging
            <span class="field__hint">minutes / critical feature</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.inprocInspectionMin}
            autocomplete="off"
            aria-invalid={fieldErrors.inproc_inspection_min !== undefined}
            data-testid="rate-inproc"
          />
          {#if fieldErrors.inproc_inspection_min !== undefined}
            <span class="field__error">{fieldErrors.inproc_inspection_min}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            CMM / final report
            <span class="field__hint">minutes / critical feature</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.cmmMinPerCriticalFeature}
            autocomplete="off"
            aria-invalid={fieldErrors.cmm_min_per_critical_feature !== undefined}
            data-testid="rate-cmm"
          />
          {#if fieldErrors.cmm_min_per_critical_feature !== undefined}
            <span class="field__error">{fieldErrors.cmm_min_per_critical_feature}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Scrap / rework uplift
            <span class="field__hint">fraction of (material + machining), e.g. 0.03</span>
          </span>
          <input
            type="number"
            step="any"
            min="0"
            bind:value={form.reworkScrapPct}
            autocomplete="off"
            aria-invalid={fieldErrors.rework_scrap_pct !== undefined}
            data-testid="rate-scrap"
          />
          {#if fieldErrors.rework_scrap_pct !== undefined}
            <span class="field__error">{fieldErrors.rework_scrap_pct}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Feed-slowdown factor
            <span class="field__hint">&ge; 1.0 — multiplies the extra finishing minutes</span>
          </span>
          <input
            type="number"
            step="any"
            min="1"
            bind:value={form.feedSlowdownFactor}
            autocomplete="off"
            aria-invalid={fieldErrors.feed_slowdown_factor !== undefined}
            data-testid="rate-feed"
          />
          {#if fieldErrors.feed_slowdown_factor !== undefined}
            <span class="field__error">{fieldErrors.feed_slowdown_factor}</span>
          {/if}
        </label>

        <label class="field field--checkbox">
          <input
            type="checkbox"
            bind:checked={form.grindingEscalation}
            data-testid="rate-grinding"
          />
          <span class="field__label">
            Grinding escalation
            <span class="field__hint">tightest-band adder (ultra-precision)</span>
          </span>
        </label>

        <label class="field">
          <span class="field__label">Notes</span>
          <input
            type="text"
            bind:value={form.notes}
            autocomplete="off"
            data-testid="rate-notes"
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
  dialog.tol-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 520px;
    overflow: hidden;
  }

  dialog.tol-form::backdrop {
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

  .field--checkbox {
    flex-direction: row;
    align-items: center;
    gap: var(--space-2);
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
    border-radius: 4px;
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .field--checkbox input {
    width: auto;
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
    border-radius: 4px;
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .primary {
    padding: var(--space-2) var(--space-5);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: 4px;
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
