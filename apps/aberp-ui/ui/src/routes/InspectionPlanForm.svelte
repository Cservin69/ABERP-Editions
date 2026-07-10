<script lang="ts">
  // S443 / ADR-0092 — inspection-plan create/edit modal. Reused for both:
  // `plan === null` opens in create mode (POST); a non-null value
  // pre-fills the form for an edit (PUT). Mirrors MachineForm.svelte:
  // native `<dialog>` modal, client-side validation + A157 inline
  // backend-error envelope, dark CSS tokens.

  import {
    createInspectionPlan,
    updateInspectionPlan,
    type InspectionPlan,
  } from "../lib/api";
  import {
    composePlanInputs,
    emptyPlanForm,
    formFromPlan,
    parsePlanValidationError,
    validatePlanForm,
    type PlanFormState,
  } from "../lib/inspection-plans";

  interface Props {
    /** `null` for create mode; a populated plan for edit mode. */
    plan: InspectionPlan | null;
    /** Invoked after a successful POST or PUT. The parent reloads the list. */
    onSaved: () => void;
    /** Invoked on Cancel / backdrop / ESC. */
    onClose: () => void;
  }

  let { plan, onSaved, onClose }: Props = $props();

  const isEdit = $derived(plan !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: PlanFormState = $state(emptyPlanForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  // Initialise the form from the plan prop on first paint. The parent
  // remounts the modal whenever its modal state flips, so this fires
  // exactly once per instance.
  $effect(() => {
    if (plan !== null) {
      form = formFromPlan(plan);
    }
  });

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;

    // Client-side validation first — surface the problem before the
    // round-trip (the backend re-checks authoritatively).
    const clientErrors = validatePlanForm(form);
    if (Object.keys(clientErrors).length > 0) {
      fieldErrors = clientErrors;
      submitError = "Some fields need attention — see the inline messages.";
      return;
    }

    fieldErrors = {};
    submitting = true;
    try {
      const body = composePlanInputs(form);
      if (plan === null) {
        await createInspectionPlan(body);
      } else {
        await updateInspectionPlan(plan.plan_id, body);
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parsePlanValidationError(message);
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
  class="plan-form"
  onclose={onDialogClose}
  onclick={onDialogClick}
  aria-label={isEdit ? "Edit inspection plan" : "New inspection plan"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>
        {isEdit
          ? "Ellenőrzési terv szerkesztése / Edit inspection plan"
          : "Új ellenőrzési terv / New inspection plan"}
      </h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    <fieldset disabled={submitting} class="body">
      <section class="column">
        <h3 class="section">Identitás / Identity</h3>

        <label class="field">
          <span class="field__label">Termék azonosító / Product id *</span>
          <input
            type="text"
            bind:value={form.productId}
            autocomplete="off"
            aria-invalid={fieldErrors.product_id !== undefined}
            data-testid="plan-product-id"
          />
          {#if fieldErrors.product_id !== undefined}
            <span class="field__error">{fieldErrors.product_id}</span>
          {/if}
        </label>

        <label class="field">
          <span class="field__label">Jellemző / Feature *</span>
          <input
            type="text"
            bind:value={form.featureName}
            autocomplete="off"
            required
            aria-invalid={fieldErrors.feature_name !== undefined}
            data-testid="plan-feature-name"
          />
          {#if fieldErrors.feature_name !== undefined}
            <span class="field__error">{fieldErrors.feature_name}</span>
          {/if}
        </label>

        <h3 class="section">Tűrés / Tolerance</h3>

        <div class="tol-grid">
          <label class="field">
            <span class="field__label">Névleges / Nominal *</span>
            <input
              type="number"
              step="any"
              bind:value={form.nominalValue}
              autocomplete="off"
              data-testid="plan-nominal"
            />
          </label>
          <label class="field">
            <span class="field__label">Alsó tűrés / Lower tol *</span>
            <input
              type="number"
              step="any"
              bind:value={form.lowerTol}
              autocomplete="off"
              aria-invalid={fieldErrors.upper_tol !== undefined}
              data-testid="plan-lower-tol"
            />
          </label>
          <label class="field">
            <span class="field__label">Felső tűrés / Upper tol *</span>
            <input
              type="number"
              step="any"
              bind:value={form.upperTol}
              autocomplete="off"
              aria-invalid={fieldErrors.upper_tol !== undefined}
              data-testid="plan-upper-tol"
            />
          </label>
        </div>
        {#if fieldErrors.upper_tol !== undefined}
          <span class="field__error">{fieldErrors.upper_tol}</span>
        {/if}

        <label class="field">
          <span class="field__label">Mértékegység / Units *</span>
          <input
            type="text"
            bind:value={form.units}
            autocomplete="off"
            required
            aria-invalid={fieldErrors.units !== undefined}
            data-testid="plan-units"
          />
          {#if fieldErrors.units !== undefined}
            <span class="field__error">{fieldErrors.units}</span>
          {/if}
        </label>

        <h3 class="section">Mérés / Measurement</h3>

        <label class="field">
          <span class="field__label">
            Szonda-ciklus / Probe cycle id
            <span class="field__hint">optional</span>
          </span>
          <input
            type="text"
            bind:value={form.optionalProbeCycleId}
            autocomplete="off"
            data-testid="plan-probe-cycle"
          />
        </label>

        <label class="field field--checkbox">
          <input
            type="checkbox"
            bind:checked={form.enabled}
            data-testid="plan-enabled"
          />
          <span class="field__label">Engedélyezve / Enabled</span>
        </label>
      </section>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not save inspection plan.</strong>
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
            {isEdit ? "Save changes" : "Create plan"}
          {/if}
        </button>
      </div>
    </fieldset>
  </form>
</dialog>

<style>
  dialog.plan-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 560px;
    overflow: hidden;
  }

  dialog.plan-form::backdrop {
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
    gap: var(--space-3);
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

  .tol-grid {
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

  .field input {
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
