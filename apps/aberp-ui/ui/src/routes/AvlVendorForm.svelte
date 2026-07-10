<script lang="ts">
  // S431 — AVL vendor create/edit modal. `vendor === null` opens in
  // create mode (POST); a non-null value pre-fills for an edit (PUT, which
  // edits categories/until/notes only — status changes go through the list
  // row's status control). Native <dialog>, inline validation, dark tokens.

  import { createAvlVendor, updateAvlVendor, type AvlVendor } from "../lib/api";
  import {
    APPROVAL_CATEGORIES,
    APPROVED_STATUSES,
    composeVendorEdit,
    composeVendorInputs,
    emptyVendorForm,
    formFromVendor,
    parseVendorValidationError,
    toggleCategory,
    type VendorFormState,
  } from "../lib/avl-vendors";

  interface Props {
    /** `null` for create mode; a populated vendor for edit mode. */
    vendor: AvlVendor | null;
    onSaved: () => void;
    onClose: () => void;
  }

  let { vendor, onSaved, onClose }: Props = $props();

  const isEdit = $derived(vendor !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: VendorFormState = $state(emptyVendorForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  $effect(() => {
    if (vendor !== null) {
      form = formFromVendor(vendor);
    }
  });

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  function toggle(category: (typeof APPROVAL_CATEGORIES)[number]["value"]) {
    form.categories = toggleCategory(form.categories, category);
  }

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    fieldErrors = {};
    submitting = true;
    try {
      if (vendor === null) {
        await createAvlVendor(composeVendorInputs(form));
      } else {
        await updateAvlVendor(vendor.id, composeVendorEdit(form));
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseVendorValidationError(message);
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
</script>

<dialog
  bind:this={dialogEl}
  class="vendor-form"
  onclose={onClose}
  onclick={onDialogClick}
  aria-label={isEdit ? "Edit vendor" : "New vendor"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>{isEdit ? "Edit AVL vendor" : "New AVL vendor"}</h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    <fieldset disabled={submitting} class="body">
      <label class="field">
        <span class="field__label">Vendor (partner id) *</span>
        <input
          type="text"
          bind:value={form.partnerId}
          autocomplete="off"
          required
          disabled={isEdit}
          aria-invalid={fieldErrors.partner_id !== undefined}
          data-testid="vendor-partner-id"
        />
        {#if isEdit}
          <span class="field__hint">The vendor reference is fixed after creation.</span>
        {/if}
        {#if fieldErrors.partner_id !== undefined}
          <span class="field__error">{fieldErrors.partner_id}</span>
        {/if}
      </label>

      {#if !isEdit}
        <label class="field">
          <span class="field__label">Initial status</span>
          <select bind:value={form.approvedStatus} data-testid="vendor-status">
            {#each APPROVED_STATUSES.filter((s) => s.value !== "revoked") as s (s.value)}
              <option value={s.value}>{s.label}</option>
            {/each}
          </select>
          {#if fieldErrors.approved_status !== undefined}
            <span class="field__error">{fieldErrors.approved_status}</span>
          {/if}
        </label>
      {/if}

      <fieldset class="field categories">
        <legend class="field__label">Approval categories</legend>
        <div class="category-grid">
          {#each APPROVAL_CATEGORIES as c (c.value)}
            <label class="check">
              <input
                type="checkbox"
                checked={form.categories.includes(c.value)}
                onchange={() => toggle(c.value)}
              />
              <span>{c.label}</span>
            </label>
          {/each}
        </div>
        {#if fieldErrors.approval_categories !== undefined}
          <span class="field__error">{fieldErrors.approval_categories}</span>
        {/if}
      </fieldset>

      <label class="field">
        <span class="field__label">
          Approved until
          <span class="field__hint">RFC-3339; blank = no expiry</span>
        </span>
        <input
          type="text"
          bind:value={form.approvedUntil}
          placeholder="2030-01-01T00:00:00Z"
          autocomplete="off"
          aria-invalid={fieldErrors.approved_until_utc !== undefined}
          data-testid="vendor-until"
        />
        {#if fieldErrors.approved_until_utc !== undefined}
          <span class="field__error">{fieldErrors.approved_until_utc}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">Screening notes</span>
        <textarea
          rows="3"
          bind:value={form.screeningNotes}
          aria-invalid={fieldErrors.screening_notes !== undefined}
          data-testid="vendor-notes"
        ></textarea>
        {#if fieldErrors.screening_notes !== undefined}
          <span class="field__error">{fieldErrors.screening_notes}</span>
        {/if}
      </label>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not save vendor.</strong>
          <p class="error__detail">{submitError}</p>
        </div>
      {/if}

      <div class="actions">
        <button type="button" class="quiet-button" onclick={onCancel}>Cancel</button>
        <button type="submit" class="primary" disabled={submitting}>
          {#if submitting}
            Saving…
          {:else}
            {isEdit ? "Save changes" : "Create vendor"}
          {/if}
        </button>
      </div>
    </fieldset>
  </form>
</dialog>

<style>
  dialog.vendor-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 560px;
    overflow: hidden;
  }

  dialog.vendor-form::backdrop {
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

  .field input:disabled {
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
    cursor: not-allowed;
  }

  .field input[aria-invalid="true"],
  .field textarea[aria-invalid="true"] {
    border-color: var(--color-signal-negative);
  }

  .categories {
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    padding: var(--space-3);
  }

  .category-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-2);
  }

  .check {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  .check input {
    width: auto;
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
