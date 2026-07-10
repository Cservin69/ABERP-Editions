<script lang="ts">
  // S431 — "Screen vendor" modal. Records a (currently mock) screening
  // against the export-control denied-party lists and fires the
  // `supplier.export_screened` audit event — the wiring is REAL even though
  // the screening itself is a no-op until an OFAC/SDN integration lands.

  import { screenAvlVendor, type AvlVendor, type ApprovalCategory, type AvlScreeningResult } from "../lib/api";
  import {
    APPROVAL_CATEGORIES,
    SCREENING_RESULTS,
    toggleCategory,
  } from "../lib/avl-vendors";

  interface Props {
    vendor: AvlVendor;
    onScreened: () => void;
    onClose: () => void;
  }

  let { vendor, onScreened, onClose }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  // Default the screened categories to the vendor's current approval set.
  let categories: ApprovalCategory[] = $state([]);
  let result: AvlScreeningResult = $state("skipped_no_integration");
  let submitting = $state(false);
  let submitError: string | null = $state(null);

  // Seed the category selection from the vendor prop on first paint (the
  // parent remounts the modal per vendor, so this fires once per instance).
  $effect(() => {
    categories = [...vendor.approval_categories];
  });

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  function toggle(category: ApprovalCategory) {
    categories = toggleCategory(categories, category);
  }

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    submitting = true;
    try {
      await screenAvlVendor(vendor.id, categories, result);
      onScreened();
    } catch (err: unknown) {
      submitError = err instanceof Error ? err.message : String(err);
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
  class="screen-modal"
  onclose={onClose}
  onclick={onDialogClick}
  aria-label="Screen vendor"
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>Screen vendor</h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    <p class="lede">
      Screen <strong class="mono">{vendor.partner_id}</strong> against the
      export-control denied-party lists. No live OFAC/SDN integration is wired
      yet — the result defaults to <em>Skipped</em>, but the screening event is
      recorded to the audit ledger.
    </p>

    <fieldset disabled={submitting} class="body">
      <fieldset class="field categories">
        <legend class="field__label">Categories screened</legend>
        <div class="category-grid">
          {#each APPROVAL_CATEGORIES as c (c.value)}
            <label class="check">
              <input
                type="checkbox"
                checked={categories.includes(c.value)}
                onchange={() => toggle(c.value)}
              />
              <span>{c.label}</span>
            </label>
          {/each}
        </div>
      </fieldset>

      <label class="field">
        <span class="field__label">Result</span>
        <select bind:value={result} data-testid="screen-result">
          {#each SCREENING_RESULTS as r (r.value)}
            <option value={r.value}>{r.label}</option>
          {/each}
        </select>
      </label>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not record screening.</strong>
          <p class="error__detail">{submitError}</p>
        </div>
      {/if}

      <div class="actions">
        <button type="button" class="quiet-button" onclick={onCancel}>Cancel</button>
        <button type="submit" class="primary" disabled={submitting}>
          {submitting ? "Recording…" : "Record screening"}
        </button>
      </div>
    </fieldset>
  </form>
</dialog>

<style>
  dialog.screen-modal {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    width: 520px;
    overflow: hidden;
  }

  dialog.screen-modal::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
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

  .lede {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
    line-height: 1.5;
  }

  .mono {
    font-family: var(--type-family-mono);
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

  .field select {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
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
