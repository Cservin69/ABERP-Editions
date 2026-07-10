<script lang="ts">
  // S428 — margin-profile create/edit modal. `profile === null` opens in
  // create mode (POST); a non-null value pre-fills for an edit (PUT).
  // Mirrors MachineForm.svelte: native <dialog>, A157 inline validation,
  // dark CSS tokens. A 409 duplicate-active-type surfaces as the generic
  // submit error.

  import {
    createMarginProfile,
    updateMarginProfile,
    type MarginProfile,
  } from "../lib/api";
  import {
    MARGIN_PROFILE_CUSTOMER_TYPES,
    composeMarginProfileInputs,
    emptyMarginProfileForm,
    formFromProfile,
    parseMarginProfileValidationError,
    type MarginProfileFormState,
  } from "../lib/margin-profiles";

  interface Props {
    /** `null` for create mode; a populated profile for edit mode. */
    profile: MarginProfile | null;
    onSaved: () => void;
    onClose: () => void;
  }

  let { profile, onSaved, onClose }: Props = $props();

  const isEdit = $derived(profile !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: MarginProfileFormState = $state(emptyMarginProfileForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  $effect(() => {
    if (profile !== null) {
      form = formFromProfile(profile);
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
      const body = composeMarginProfileInputs(form);
      if (profile === null) {
        await createMarginProfile(body);
      } else {
        await updateMarginProfile(profile.id, body);
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseMarginProfileValidationError(message);
      if (typed !== null) {
        const next: Record<string, string> = {};
        for (const f of typed.fields) {
          next[f.field] = f.message;
        }
        fieldErrors = next;
        submitError = "Some fields need attention — see the inline messages.";
      } else if (message.includes("already exists for that customer type")) {
        submitError =
          "An active margin profile already exists for that customer type. Archive it first, or edit the existing one.";
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
  class="profile-form"
  onclose={onDialogClose}
  onclick={onDialogClick}
  aria-label={isEdit ? "Edit margin profile" : "New margin profile"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>{isEdit ? "Edit margin profile" : "New margin profile"}</h2>
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
        <label class="field">
          <span class="field__label">Name *</span>
          <input
            type="text"
            bind:value={form.name}
            autocomplete="off"
            required
            aria-invalid={fieldErrors.name !== undefined}
            data-testid="profile-name"
          />
          {#if fieldErrors.name !== undefined}
            <span class="field__error">{fieldErrors.name}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">Customer type *</span>
          <select bind:value={form.customerType} data-testid="profile-type">
            {#each MARGIN_PROFILE_CUSTOMER_TYPES as opt (opt.value)}
              <option value={opt.value}>{opt.label}</option>
            {/each}
          </select>
          {#if fieldErrors.customer_type !== undefined}
            <span class="field__error">{fieldErrors.customer_type}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Target margin %
            <span class="field__hint">applied as the markup</span>
          </span>
          <input
            type="number"
            step="any"
            bind:value={form.grossMarginPct}
            autocomplete="off"
            aria-invalid={fieldErrors.gross_margin_pct !== undefined}
            data-testid="profile-gross"
          />
          {#if fieldErrors.gross_margin_pct !== undefined}
            <span class="field__error">{fieldErrors.gross_margin_pct}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Minimum margin % (floor)
            <span class="field__hint">DEAL refused below this</span>
          </span>
          <input
            type="number"
            step="any"
            bind:value={form.minMarginPct}
            autocomplete="off"
            aria-invalid={fieldErrors.min_margin_pct !== undefined}
            data-testid="profile-min"
          />
          {#if fieldErrors.min_margin_pct !== undefined}
            <span class="field__error">{fieldErrors.min_margin_pct}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">
            Notes
            <span class="field__hint">optional</span>
          </span>
          <textarea
            bind:value={form.notes}
            rows="2"
            autocomplete="off"
            data-testid="profile-notes"
          ></textarea>
        </label>

        <label class="field field--checkbox">
          <input
            type="checkbox"
            bind:checked={form.enabled}
            data-testid="profile-enabled"
          />
          <span class="field__label">Enabled</span>
        </label>
      </section>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not save margin profile.</strong>
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
            {isEdit ? "Save changes" : "Create profile"}
          {/if}
        </button>
      </div>
    </fieldset>
  </form>
</dialog>

<style>
  dialog.profile-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 520px;
    overflow: hidden;
  }

  dialog.profile-form::backdrop {
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
  .field select,
  .field textarea {
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
