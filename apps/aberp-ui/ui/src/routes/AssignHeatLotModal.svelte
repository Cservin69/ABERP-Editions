<script lang="ts">
  // S432 — "Assign heat lot" modal. Stamps a heat-lot number (+ optional
  // mill-test-report URL) onto a material grade's inventory balance row.
  // One form, one save, no decisions ([[hulye-biztos]]). Client-side
  // pre-validation mirrors the backend (which stays authoritative).

  import { assignHeatLot, type InventoryBalance } from "../lib/api";
  import { validateHeatLot, validateMtrUrl } from "../lib/heat-lot";

  interface Props {
    balance: InventoryBalance;
    onAssigned: () => void;
    onClose: () => void;
  }

  let { balance, onAssigned, onClose }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  let heatLot = $state("");
  let mtrUrl = $state("");
  let submitting = $state(false);
  let submitError: string | null = $state(null);

  // Client-side pre-validation (backend re-checks authoritatively).
  let heatLotError = $derived(validateHeatLot(heatLot));
  let mtrUrlError = $derived(validateMtrUrl(mtrUrl));
  let canSave = $derived(heatLotError === null && mtrUrlError === null);

  // Seed from the existing assignment so a re-stamp pre-fills the form.
  // The parent remounts the modal per row (keyed on grade), so this
  // fires once per instance.
  $effect(() => {
    heatLot = balance.heat_lot_number ?? "";
    mtrUrl = balance.mill_test_report_url ?? "";
  });

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  async function onSubmit(event: Event) {
    event.preventDefault();
    if (!canSave) return;
    submitError = null;
    submitting = true;
    try {
      const mtr = mtrUrl.trim() === "" ? null : mtrUrl.trim();
      await assignHeatLot(balance.material_grade, heatLot.trim(), mtr);
      onAssigned();
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
  class="heat-modal"
  onclose={onClose}
  onclick={onDialogClick}
  aria-label="Assign heat lot"
>
  <form class="frame" onsubmit={onSubmit}>
    <header class="head">
      <h2>Hőkezelési tétel / Heat lot</h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    <p class="lede">
      Stamp a heat lot onto <strong class="mono">{balance.material_grade}</strong>.
      The mill test report (MTR) URL is optional — a
      <code>file://</code> path to the certificate, or leave it empty.
    </p>

    <fieldset disabled={submitting} class="body">
      <label class="field">
        <span class="field__label">Heat lot number / Tétel</span>
        <input
          type="text"
          bind:value={heatLot}
          data-testid="heat-lot-input"
          placeholder="HL-2026-007"
          autocomplete="off"
        />
        {#if heatLotError !== null}
          <span class="field__error">{heatLotError}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">Mill test report URL (optional)</span>
        <input
          type="text"
          bind:value={mtrUrl}
          data-testid="mtr-url-input"
          placeholder="file:// path or leave empty"
          autocomplete="off"
        />
        {#if mtrUrlError !== null}
          <span class="field__error">{mtrUrlError}</span>
        {/if}
      </label>

      {#if submitError !== null}
        <div class="error" role="alert">
          <strong>Could not assign heat lot.</strong>
          <p class="error__detail">{submitError}</p>
        </div>
      {/if}

      <div class="actions">
        <button type="button" class="quiet-button" onclick={onCancel}>Cancel</button>
        <button type="submit" class="primary" disabled={submitting || !canSave}>
          {submitting ? "Saving…" : "Assign / Mentés"}
        </button>
      </div>
    </fieldset>
  </form>
</dialog>

<style>
  dialog.heat-modal {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    width: 520px;
    overflow: hidden;
  }

  dialog.heat-modal::backdrop {
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

  .field input {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .field__error {
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
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
