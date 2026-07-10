<script lang="ts">
  // PR-91 — Product create/edit modal. Mirrors PartnerForm shape +
  // validation-envelope handling (A157 inline-error rendering). The
  // unit-of-measure dropdown surfaces NAV's enum tokens with
  // Hungarian labels + a sentinel "Egyéb (Own)" option that reveals
  // a free-text input — the OWN escape hatch (e.g. liter@15C) per
  // ADR-0046.
  //
  // Price input uses PR-88's text + inputmode="decimal" + the
  // `parseAmountToMinor` rule (bare ints = WHOLE major units, `.`
  // and `,` both accepted, spaces stripped) so EUR catalog entries
  // can't trip the cents-shift bug Ervin caught.

  import { createProduct, updateProduct, type Product } from "../lib/api";
  import {
    composeProductInputs,
    emptyProductForm,
    formFromProduct,
    NAV_UNIT_OPTIONS,
    OWN_UNIT_SENTINEL,
    parseProductValidationError,
    type ProductFormState,
  } from "../lib/products";

  interface Props {
    /** `null` for create mode; populated Product for edit mode. */
    product: Product | null;
    onSaved: () => void;
    onClose: () => void;
  }

  let { product, onSaved, onClose }: Props = $props();

  const isEdit = $derived(product !== null);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: ProductFormState = $state(emptyProductForm());
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  $effect(() => {
    if (product !== null) {
      form = formFromProduct(product);
    }
  });

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  const isOwnSelected = $derived(form.unitSelection === OWN_UNIT_SENTINEL);

  const priceLabelHint = $derived(
    form.currency === "HUF"
      ? "Unit price (HUF, whole forints)"
      : "Unit price (EUR, e.g. 340 or 340,50)",
  );

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    fieldErrors = {};
    submitting = true;
    try {
      const body = composeProductInputs(form);
      if (product === null) {
        await createProduct(body);
      } else {
        await updateProduct(product.id, body);
      }
      onSaved();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseProductValidationError(message);
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
  class="product-form"
  onclose={onDialogClose}
  onclick={onDialogClick}
  aria-label={isEdit ? "Edit product" : "New product"}
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>{isEdit ? "Edit product" : "New product"}</h2>
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
      <label class="field">
        <span class="field__label">Name *</span>
        <input
          type="text"
          bind:value={form.name}
          autocomplete="off"
          required
          aria-invalid={fieldErrors.name !== undefined}
        />
        {#if fieldErrors.name !== undefined}
          <span class="field__error">{fieldErrors.name}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">
          Unit of measure *
          <span class="field__hint">
            NAV's enum where one exists; <em>Egyéb (Own)</em> for custom labels
          </span>
        </span>
        <select bind:value={form.unitSelection}>
          {#each NAV_UNIT_OPTIONS as opt (opt.token)}
            <option value={opt.token}>{opt.label_hu} ({opt.label_en})</option>
          {/each}
          <option value={OWN_UNIT_SENTINEL}>Egyéb (Own — free text)</option>
        </select>
        {#if fieldErrors.unit !== undefined}
          <span class="field__error">{fieldErrors.unit}</span>
        {/if}
      </label>

      {#if isOwnSelected}
        <label class="field">
          <span class="field__label">
            Own unit label *
            <span class="field__hint">
              e.g. <code>liter@15C</code> (temperature-corrected litre)
            </span>
          </span>
          <input
            type="text"
            bind:value={form.unitOwnLabel}
            autocomplete="off"
            spellcheck="false"
            placeholder="liter@15C"
            required
            aria-invalid={fieldErrors.unit !== undefined}
          />
        </label>
      {/if}

      <label class="field">
        <span class="field__label">Currency *</span>
        <select bind:value={form.currency}>
          <option value="HUF">HUF (forint)</option>
          <option value="EUR">EUR (euro)</option>
        </select>
      </label>

      <label class="field">
        <span class="field__label">
          {priceLabelHint}
          <span class="field__hint">
            bare integer = whole {form.currency === "HUF"
              ? "forints"
              : "euros"}
          </span>
        </span>
        <input
          type="text"
          inputmode="decimal"
          bind:value={form.unitPriceInput}
          autocomplete="off"
          spellcheck="false"
          placeholder={form.currency === "HUF" ? "25000" : "340 or 340,50"}
          required
          aria-invalid={fieldErrors.unit_price_minor !== undefined}
        />
        {#if fieldErrors.unit_price_minor !== undefined}
          <span class="field__error">{fieldErrors.unit_price_minor}</span>
        {/if}
      </label>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not save product.</strong>
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
            {isEdit ? "Save changes" : "Create product"}
          {/if}
        </button>
      </div>
    </fieldset>
  </form>
</dialog>

<style>
  dialog.product-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 560px;
    overflow: hidden;
  }

  dialog.product-form::backdrop {
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
  .field select {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
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
