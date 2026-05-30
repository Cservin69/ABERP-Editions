<script lang="ts">
  // S174 / PR-174 — shared line-row fields for the SPA's invoice forms.
  //
  // Today this component is consumed by `ModificationInvoice.svelte`
  // ONLY. The `IssueInvoice.svelte` line markup carries hardened
  // PR-100 / S159 wiring (saved-product combobox dropdown around the
  // description input + `productCurrencyAtPick` chip + per-line
  // `NotesAutocomplete`) that intentionally does NOT migrate in this
  // PR — Issue's `.line` div is the operator-facing entry point for
  // the regulatory issuance path and any refactor that bundles those
  // affordances into a shared component risks an HTML-output diff that
  // would surface as a behavioural change (CLAUDE.md rule 11 — match
  // the codebase conventions; rule 3 — surgical changes only).
  //
  // The shape this component owns is the COMMON SUBSET both forms
  // render verbatim: description (plain text), decimal quantity,
  // decimal unit-price, integer VAT %, remove affordance. The per-
  // field preflight error block (HU + EN message + `input-invalid`
  // border) is built in so any future consumer wires inline errors
  // for free.
  //
  // Future S91-followon may extract Issue's product-combobox into a
  // `descriptionSlot` snippet and migrate Issue's `.line` to consume
  // this component too — at which point the testid prefix + the
  // existing inline-error wiring already match. For now the component
  // is intentionally minimal so Modify gets the same hardened error-
  // routing surface Issue has carried since S91 without expanding
  // Issue's blast radius.

  import type { Currency } from "./api";
  import type { InvoicePreflightErrorItem } from "./issue-invoice";

  interface Props {
    /** Per-line description; plain text input (NO product combobox in
     * this component — see header). Two-way bound. */
    description: string;
    /** Operator-typed quantity string. Decimal-tolerant (`1.5` or `1,5`);
     * parent's composer parses it via `parseDecimalQuantity` at submit. */
    quantityInput: string;
    /** Operator-typed unit price string. Decimal-tolerant; parent's
     * composer parses via `parseAmountToMinor` (the form's currency
     * drives major→minor scaling). */
    unitPriceInput: string;
    /** Integer VAT rate percent. Bound to a `<input type="number">`. */
    vatRatePercent: number;
    /** Form currency. Drives the unit-price placeholder + label hint
     * so HUF lines show "whole forints" and EUR lines show "340,50". */
    currency: Currency;
    /** Zero-based line index. Used for aria-labels + testid suffixes. */
    index: number;
    /** `true` when more than one line exists — drives the remove
     * button's enabled state. The single-remaining-line guard is the
     * canonical "at least one line is required" surface (Issue and
     * Modify both gate it at the parent's `removeLine`). */
    removable: boolean;
    /** Invoked on remove-button click. Parent handles the splice. */
    onRemove: () => void;
    /** Optional per-field preflight errors. Keys are the four field
     * names; absent keys render no inline error. */
    errors?: Partial<
      Record<
        "description" | "quantity" | "unitPrice" | "vatRatePercent",
        InvoicePreflightErrorItem
      >
    >;
    /** Optional testid prefix; defaults to `"line"` so Issue's existing
     * `line-0-description-input` selectors keep working when Issue
     * eventually migrates to this component. */
    testidPrefix?: string;
  }

  let {
    description = $bindable(""),
    quantityInput = $bindable(""),
    unitPriceInput = $bindable(""),
    vatRatePercent = $bindable(27),
    currency,
    index,
    removable,
    onRemove,
    errors,
    testidPrefix = "line",
  }: Props = $props();

  const descriptionError = $derived(errors?.description);
  const quantityError = $derived(errors?.quantity);
  const unitPriceError = $derived(errors?.unitPrice);
  const vatError = $derived(errors?.vatRatePercent);
</script>

<div class="line">
  <label class="wide">
    <span>Description</span>
    <input
      type="text"
      bind:value={description}
      required
      class:input-invalid={descriptionError !== undefined}
      aria-invalid={descriptionError !== undefined}
      data-testid={`${testidPrefix}-${index}-description-input`}
    />
  </label>
  <label class="narrow">
    <span>Qty</span>
    <input
      type="text"
      inputmode="decimal"
      autocomplete="off"
      spellcheck="false"
      bind:value={quantityInput}
      required
      placeholder="1,5"
      class:input-invalid={quantityError !== undefined}
      aria-invalid={quantityError !== undefined}
      data-testid={`${testidPrefix}-${index}-quantity-input`}
    />
  </label>
  <label class="narrow">
    <span>
      Unit price ({currency === "EUR"
        ? "EUR, e.g. 340 or 340,50"
        : "HUF, whole forints"})
    </span>
    <input
      type="text"
      inputmode="decimal"
      autocomplete="off"
      spellcheck="false"
      bind:value={unitPriceInput}
      required
      placeholder={currency === "EUR" ? "340,50" : "340000"}
      class:input-invalid={unitPriceError !== undefined}
      aria-invalid={unitPriceError !== undefined}
      data-testid={`${testidPrefix}-${index}-unit-price-input`}
    />
  </label>
  <label class="narrow">
    <span>VAT %</span>
    <input
      type="number"
      min="0"
      max="100"
      step="1"
      bind:value={vatRatePercent}
      required
      class:input-invalid={vatError !== undefined}
      aria-invalid={vatError !== undefined}
      data-testid={`${testidPrefix}-${index}-vat-input`}
    />
  </label>
  <button
    type="button"
    class="quiet-button line-remove"
    onclick={onRemove}
    disabled={!removable}
    aria-label={`Remove line ${index + 1}`}
    title={removable
      ? `Remove line ${index + 1}`
      : "At least one line is required"}
  >
    ✕
  </button>
</div>

{#if errors && (descriptionError || quantityError || unitPriceError || vatError)}
  <div class="line-errors" data-testid={`${testidPrefix}-${index}-errors`}>
    {#if descriptionError}
      <p
        class="inline-error"
        data-testid={`${testidPrefix}-${index}-description-error`}
        data-kind={descriptionError.kind}
      >
        <span class="inline-error-hu">{descriptionError.message_hu}</span>
        <span class="inline-error-en">{descriptionError.message_en}</span>
      </p>
    {/if}
    {#if quantityError}
      <p
        class="inline-error"
        data-testid={`${testidPrefix}-${index}-quantity-error`}
        data-kind={quantityError.kind}
      >
        <span class="inline-error-hu">{quantityError.message_hu}</span>
        <span class="inline-error-en">{quantityError.message_en}</span>
      </p>
    {/if}
    {#if unitPriceError}
      <p
        class="inline-error"
        data-testid={`${testidPrefix}-${index}-unitPrice-error`}
        data-kind={unitPriceError.kind}
      >
        <span class="inline-error-hu">{unitPriceError.message_hu}</span>
        <span class="inline-error-en">{unitPriceError.message_en}</span>
      </p>
    {/if}
    {#if vatError}
      <p
        class="inline-error"
        data-testid={`${testidPrefix}-${index}-vatRatePercent-error`}
        data-kind={vatError.kind}
      >
        <span class="inline-error-hu">{vatError.message_hu}</span>
        <span class="inline-error-en">{vatError.message_en}</span>
      </p>
    {/if}
  </div>
{/if}

<style>
  /* Layout mirrors `IssueInvoice.svelte` and `ModificationInvoice.svelte`'s
   * pre-S174 `.line` / `.line-errors` selectors — same token vars, same
   * flex shape, so the migrated parent renders byte-identical chrome to
   * what the operator had before the extraction. */

  label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
    flex: 1 1 auto;
  }

  label.narrow {
    flex: 0 0 8ch;
  }

  label.wide {
    flex: 2 1 auto;
  }

  .line {
    display: flex;
    gap: var(--space-2);
    align-items: flex-end;
  }

  input[type="text"],
  input[type="number"] {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  input:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 1px;
    border-color: var(--color-text-muted);
  }

  input.input-invalid {
    border-color: var(--color-signal-negative);
    outline-color: var(--color-signal-negative);
  }

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    transition: color var(--motion-fade-in);
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quiet-button:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }

  .line-remove {
    flex: 0 0 auto;
    align-self: flex-end;
  }

  .line-errors {
    margin-bottom: var(--space-2);
  }

  .inline-error {
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
  }

  .inline-error-hu {
    color: var(--color-signal-negative);
  }

  .inline-error-en {
    color: var(--color-text-muted);
    font-style: italic;
  }
</style>
