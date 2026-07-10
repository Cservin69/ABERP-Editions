<script lang="ts">
  // S239 / PR-233 — shared confirmation modal for destructive row
  // actions. Replaces ad-hoc `window.confirm` per the S237 §🟡 #3
  // finding (browser-native dialogs cannot be styled with tokens.css
  // and look wrong against the dark chrome) and the load-bearing
  // [[storno-workflow-adr0049]] precedent that mandated "row-action
  // opens modal not window.confirm."
  //
  // Used today by InvoiceList for the Draft delete flow. Wider Phase γ
  // adoption (WO Cancel/Hold prompts, QA Fail/Rework reason input,
  // Dispatch Cancel confirm — the three other surfaces S237 §🟡 #3
  // flagged) is named-deferred to follow-up PRs — those need an
  // optional reason textarea this component can grow into when the
  // first such consumer lands.
  //
  // Token-styled per [[spa-dark-theme-default]] — every property
  // resolves to a defined token in `apps/aberp-ui/ui/src/lib/tokens.css`.
  // No `--color-surface` / `--color-primary` / `--color-danger`
  // shorthand (those legacy tokens fall back to bright defaults).
  //
  // Pattern mirrors `PartnerForm.svelte`'s `dialog.partner-form`
  // frame; the diff is purely shape (confirmation, no form fields)
  // and danger affordance (the primary button uses
  // `--color-signal-negative` to mark the destructive intent).

  interface Props {
    /** Modal title (short label). */
    title: string;
    /** Primary body sentence — explains what is about to happen. */
    body: string;
    /** Optional secondary sentence — only rendered when non-null.
     * Used for the dispatch-link consequence warning. */
    consequence?: string | null;
    /** Label for the destructive confirm button. */
    confirmLabel: string;
    /** Label for the cancel button. */
    cancelLabel: string;
    /** Disable both buttons + show the busy state. Driven by the
     * caller's "in-flight DELETE" gate so a double-click can't fire
     * the API twice. */
    busy?: boolean;
    /** Invoked when the operator clicks Confirm. The parent runs the
     * destructive call; the parent flips `busy` while the call is
     * in-flight; the parent closes the modal on completion (success
     * or error) and surfaces any error via its own A157 inline
     * pattern. */
    onConfirm: () => void;
    /** Invoked on Cancel / backdrop / ESC. The parent toggles modal
     * state to closed. */
    onCancel: () => void;
  }

  let {
    title,
    body,
    consequence = null,
    confirmLabel,
    cancelLabel,
    busy = false,
    onConfirm,
    onCancel,
  }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  function handleCancel() {
    if (busy) return;
    if (dialogEl?.open) dialogEl.close();
    onCancel();
  }

  function handleConfirm() {
    if (busy) return;
    onConfirm();
  }

  function onDialogClick(event: MouseEvent) {
    if (event.target === dialogEl) {
      handleCancel();
    }
  }

  function onDialogClose() {
    if (!busy) {
      onCancel();
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="confirm-action"
  onclose={onDialogClose}
  onclick={onDialogClick}
  aria-label={title}
>
  <div class="frame">
    <header class="head">
      <h2>{title}</h2>
    </header>

    <div class="body">
      <p class="body__primary">{body}</p>
      {#if consequence !== null}
        <p class="body__consequence" role="alert">{consequence}</p>
      {/if}
    </div>

    <footer class="actions">
      <button
        type="button"
        class="quiet-button"
        onclick={handleCancel}
        disabled={busy}
      >
        {cancelLabel}
      </button>
      <button
        type="button"
        class="danger"
        onclick={handleConfirm}
        disabled={busy}
        aria-busy={busy}
      >
        {confirmLabel}
      </button>
    </footer>
  </div>
</dialog>

<style>
  dialog.confirm-action {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    width: 480px;
    overflow: hidden;
  }

  dialog.confirm-action::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
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
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .body__primary {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
    line-height: 1.5;
  }

  .body__consequence {
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-warning);
    background: var(--color-surface-raised);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
    line-height: 1.5;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
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

  .danger {
    padding: var(--space-2) var(--space-5);
    background: var(--color-signal-negative);
    color: var(--color-surface-base);
    border: 0;
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .danger:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
