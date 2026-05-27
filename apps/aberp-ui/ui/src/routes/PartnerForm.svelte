<script lang="ts">
  // PR-54 / session-74 — Partner create/edit modal. Reused for both
  // operations: `partner === null` opens in create mode (POST); a
  // non-null value pre-fills the form for an edit (PUT).
  //
  // The form mirrors the field shape exactly from
  // `aberp::partners::PartnerInputs`. Validation envelope shape
  // (`{error: "validation_failed", fields: [...]}`) is the A157 pattern
  // already used by the seller-config wizard + the NAV-credentials
  // setup; the parser lives in `lib/partners.ts` and the renderer is
  // inline below.
  //
  // Two-column layout matching TenantSettings.svelte's pattern: identity
  // + address on the left, bank + contact + kind on the right. The
  // mandatory fields are starred in the label; the validation error
  // surfaces under the field on the next round-trip.

  import { createPartner, updatePartner, type Partner } from "../lib/api";
  import {
    composePartnerInputs,
    emptyPartnerForm,
    formFromPartner,
    parsePartnerValidationError,
    type PartnerFormState,
  } from "../lib/partners";

  interface Props {
    /** `null` for create mode; a populated Partner for edit mode. */
    partner: Partner | null;
    /** Invoked after a successful POST or PUT. The parent reloads the
     * list so the row appears (or moves to the right ordered position
     * after a rename). */
    onSaved: () => void;
    /** Invoked on Cancel / backdrop / ESC. The parent toggles modal
     * state to `null`. */
    onClose: () => void;
  }

  let { partner, onSaved, onClose }: Props = $props();

  const isEdit = $derived(partner !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: PartnerFormState = $state(emptyPartnerForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  // Initialise the form from the partner prop on first paint. The
  // parent remounts the modal whenever `modalState` flips, so this
  // effect fires exactly once per instance.
  $effect(() => {
    if (partner !== null) {
      form = formFromPartner(partner);
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
      const body = composePartnerInputs(form);
      if (partner === null) {
        await createPartner(body);
      } else {
        await updatePartner(partner.id, body);
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parsePartnerValidationError(message);
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
  class="partner-form"
  onclose={onDialogClose}
  onclick={onDialogClick}
  aria-label={isEdit ? "Edit partner" : "New partner"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>{isEdit ? "Edit partner" : "New partner"}</h2>
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
      <div class="columns">
        <section class="column">
          <h3 class="section">Identity</h3>

          <label class="field">
            <span class="field__label">Display name *</span>
            <input
              type="text"
              bind:value={form.displayName}
              autocomplete="off"
              required
              aria-invalid={fieldErrors.display_name !== undefined}
            />
            {#if fieldErrors.display_name !== undefined}
              <span class="field__error">{fieldErrors.display_name}</span>
            {/if}
          </label>

          <label class="field">
            <span class="field__label">Legal name *</span>
            <input
              type="text"
              bind:value={form.legalName}
              autocomplete="organization"
              required
              aria-invalid={fieldErrors.legal_name !== undefined}
            />
            {#if fieldErrors.legal_name !== undefined}
              <span class="field__error">{fieldErrors.legal_name}</span>
            {/if}
          </label>

          <label class="field">
            <span class="field__label">
              Tax number (ADÓSZÁM) *
              <span class="field__hint">format: <code>xxxxxxxx-y-zz</code></span>
            </span>
            <input
              type="text"
              bind:value={form.taxNumber}
              autocomplete="off"
              spellcheck="false"
              required
              placeholder="12345678-1-42"
              aria-invalid={fieldErrors.tax_number !== undefined}
            />
            {#if fieldErrors.tax_number !== undefined}
              <span class="field__error">{fieldErrors.tax_number}</span>
            {/if}
          </label>

          <label class="field">
            <span class="field__label">
              Kind *
              <span class="field__hint">
                Customer = buyer; Supplier = seller; Both = on both sides
              </span>
            </span>
            <select bind:value={form.kind}>
              <option value="Customer">Customer</option>
              <option value="Supplier">Supplier</option>
              <option value="Both">Both</option>
            </select>
          </label>

          <label class="field">
            <span class="field__label">
              EU VAT number
              <span class="field__hint">optional</span>
            </span>
            <input
              type="text"
              bind:value={form.euVatNumber}
              autocomplete="off"
              spellcheck="false"
            />
          </label>

          <h3 class="section">Address</h3>

          <label class="field">
            <span class="field__label">Street</span>
            <input
              type="text"
              bind:value={form.addressStreet}
              autocomplete="street-address"
            />
          </label>

          <label class="field">
            <span class="field__label">Postal code</span>
            <input
              type="text"
              bind:value={form.addressPostalCode}
              autocomplete="postal-code"
            />
          </label>

          <label class="field">
            <span class="field__label">City</span>
            <input
              type="text"
              bind:value={form.addressCity}
              autocomplete="address-level2"
            />
          </label>

          <label class="field">
            <span class="field__label">Country</span>
            <input
              type="text"
              bind:value={form.addressCountry}
              autocomplete="country-name"
            />
          </label>
        </section>

        <section class="column">
          <h3 class="section">
            Bank info
            <span class="section__hint">optional</span>
          </h3>

          <label class="field">
            <span class="field__label">Bank account number</span>
            <input
              type="text"
              bind:value={form.bankAccount}
              autocomplete="off"
              spellcheck="false"
            />
          </label>

          <h3 class="section">Contact</h3>

          <label class="field">
            <span class="field__label">Email</span>
            <input
              type="email"
              bind:value={form.contactEmail}
              autocomplete="email"
            />
          </label>

          <label class="field">
            <span class="field__label">Phone</span>
            <input
              type="tel"
              bind:value={form.contactPhone}
              autocomplete="tel"
            />
          </label>
        </section>
      </div>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not save partner.</strong>
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
            {isEdit ? "Save changes" : "Create partner"}
          {/if}
        </button>
      </div>
    </fieldset>
  </form>
</dialog>

<style>
  dialog.partner-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 960px;
    overflow: hidden;
  }

  dialog.partner-form::backdrop {
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

  .columns {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-5);
  }

  @media (max-width: 720px) {
    .columns {
      grid-template-columns: 1fr;
    }
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
    border-radius: 4px;
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .field input[aria-invalid="true"] {
    border-color: var(--color-signal-negative);
  }

  .field__error {
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
  }

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
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
