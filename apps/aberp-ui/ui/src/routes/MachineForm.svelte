<script lang="ts">
  // S427 — quoting-machine create/edit modal. Reused for both:
  // `machine === null` opens in create mode (POST); a non-null value
  // pre-fills the form for an edit (PUT). Mirrors PartnerForm.svelte:
  // native `<dialog>` modal, A157 validation-envelope inline errors,
  // dark CSS tokens.

  import {
    createMachine,
    updateMachine,
    type QuotingMachine,
  } from "../lib/api";
  import {
    MACHINE_FAMILIES,
    composeMachineInputs,
    emptyMachineForm,
    formFromMachine,
    parseMachineValidationError,
    type MachineFormState,
  } from "../lib/machines";

  interface Props {
    /** `null` for create mode; a populated machine for edit mode. */
    machine: QuotingMachine | null;
    /** Invoked after a successful POST or PUT. The parent reloads the
     * list so the row appears (or updates in place). */
    onSaved: () => void;
    /** Invoked on Cancel / backdrop / ESC. */
    onClose: () => void;
  }

  let { machine, onSaved, onClose }: Props = $props();

  const isEdit = $derived(machine !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: MachineFormState = $state(emptyMachineForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  // Initialise the form from the machine prop on first paint. The
  // parent remounts the modal whenever its modal state flips, so this
  // effect fires exactly once per instance.
  $effect(() => {
    if (machine !== null) {
      form = formFromMachine(machine);
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
      const body = composeMachineInputs(form);
      if (machine === null) {
        await createMachine(body);
      } else {
        await updateMachine(machine.id, body);
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseMachineValidationError(message);
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
  aria-label={isEdit ? "Edit machine" : "New machine"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>{isEdit ? "Edit machine" : "New machine"}</h2>
      <button
        type="button"
        class="quiet-button"
        onclick={onCancel}
        aria-label="Cancel"
      >
        Cancel
      </button>
    </header>

    <fieldset disabled={submitting} class="body">
      <section class="column">
        <h3 class="section">Identity</h3>

        <label class="field">
          <span class="field__label">Name *</span>
          <input
            type="text"
            bind:value={form.name}
            autocomplete="off"
            required
            aria-invalid={fieldErrors.name !== undefined}
            data-testid="machine-name"
          />
          {#if fieldErrors.name !== undefined}
            <span class="field__error">{fieldErrors.name}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">Family *</span>
          <select bind:value={form.family} data-testid="machine-family">
            {#each MACHINE_FAMILIES as fam (fam.value)}
              <option value={fam.value}>{fam.label}</option>
            {/each}
          </select>
          {#if fieldErrors.family !== undefined}
            <span class="field__error">{fieldErrors.family}</span>
          {/if}
        </label>

        <h3 class="section">
          Max envelope (mm)
          <span class="section__hint">X × Y × Z</span>
        </h3>

        <div class="envelope">
          <label class="field">
            <span class="field__label">X</span>
            <input
              type="number"
              step="any"
              bind:value={form.envelopeX}
              autocomplete="off"
              aria-invalid={fieldErrors.max_envelope_xyz_mm !== undefined}
              data-testid="machine-envelope-x"
            />
          </label>
          <label class="field">
            <span class="field__label">Y</span>
            <input
              type="number"
              step="any"
              bind:value={form.envelopeY}
              autocomplete="off"
              aria-invalid={fieldErrors.max_envelope_xyz_mm !== undefined}
              data-testid="machine-envelope-y"
            />
          </label>
          <label class="field">
            <span class="field__label">Z</span>
            <input
              type="number"
              step="any"
              bind:value={form.envelopeZ}
              autocomplete="off"
              aria-invalid={fieldErrors.max_envelope_xyz_mm !== undefined}
              data-testid="machine-envelope-z"
            />
          </label>
        </div>
        {#if fieldErrors.max_envelope_xyz_mm !== undefined}
          <span class="field__error">{fieldErrors.max_envelope_xyz_mm}</span>
        {/if}

        <h3 class="section">Capacity</h3>

        <label class="field">
          <span class="field__label">
            Daily hours available
            <span class="field__hint">hours/day</span>
          </span>
          <input
            type="number"
            step="any"
            bind:value={form.dailyHoursAvail}
            autocomplete="off"
            aria-invalid={fieldErrors.daily_hours_avail !== undefined}
            data-testid="machine-daily-hours"
          />
          {#if fieldErrors.daily_hours_avail !== undefined}
            <span class="field__error">{fieldErrors.daily_hours_avail}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Buffer %
            <span class="field__hint">capacity safety margin</span>
          </span>
          <input
            type="number"
            step="any"
            bind:value={form.bufferPct}
            autocomplete="off"
            aria-invalid={fieldErrors.buffer_pct !== undefined}
            data-testid="machine-buffer-pct"
          />
          {#if fieldErrors.buffer_pct !== undefined}
            <span class="field__error">{fieldErrors.buffer_pct}</span>
          {/if}
        </label>

        <label class="field field--checkbox">
          <input
            type="checkbox"
            bind:checked={form.enabled}
            data-testid="machine-enabled"
          />
          <span class="field__label">Enabled</span>
        </label>
      </section>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not save machine.</strong>
          <p class="error__detail">{submitError}</p>
        </div>
      {/if}

      <div class="actions">
        <button type="button" class="quiet-button" onclick={onCancel}>
          Cancel
        </button>
        <button type="submit" class="primary" disabled={submitting}>
          {#if submitting}
            Saving…
          {:else}
            {isEdit ? "Save changes" : "Create machine"}
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
    width: 560px;
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

  .section__hint {
    font-weight: 400;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    margin-left: var(--space-2);
  }

  .envelope {
    display: grid;
    grid-template-columns: 1fr 1fr 1fr;
    gap: var(--space-3);
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
    border-radius: var(--radius-sm);
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
