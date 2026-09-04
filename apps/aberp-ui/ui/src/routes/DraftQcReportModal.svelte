<script lang="ts">
  // ADR-0199 — draft a QC/AS9102 FAIR or CoC report against a work order.
  // Native <dialog> modal (mirrors InspectionPlanForm). The kind dropdown
  // uses the DRAFTABLE_KINDS triples so the wire/storage token split (CoC =
  // "coc") never leaks into the markup; an empty template resolves the
  // partner default server-side. Disposition and all counts are computed by
  // the backend at draft time — this form carries none of them.

  import { draftQcReport } from "../lib/api";
  import {
    DRAFTABLE_KINDS,
    composeDraftBody,
    emptyDraftForm,
    qcErrorMessage,
    validateDraftForm,
    type DraftFormState,
  } from "../lib/qc-reports";

  interface Props {
    /** The work order to draft against (must already have a dispatch). */
    woId: string;
    /** A human label for the WO/delivery, shown in the modal header. */
    woLabel: string;
    /** Invoked after a successful draft. The parent reloads. */
    onSaved: () => void;
    /** Invoked on Cancel / backdrop / ESC. */
    onClose: () => void;
  }

  let { woId, woLabel, onSaved, onClose }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: DraftFormState = $state(emptyDraftForm(""));
  let submitting = $state(false);

  // The modal is remounted per open, so `woId` is stable for this instance;
  // sync it onto the form state (satisfies the reactive-capture lint).
  $effect(() => {
    form.woId = woId;
  });
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    const clientErrors = validateDraftForm(form);
    if (Object.keys(clientErrors).length > 0) {
      fieldErrors = clientErrors;
      return;
    }
    fieldErrors = {};
    submitting = true;
    try {
      await draftQcReport(composeDraftBody(form));
      onSaved();
    } catch (err: unknown) {
      submitError = qcErrorMessage(err instanceof Error ? err.message : String(err));
    } finally {
      submitting = false;
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="modal"
  onclose={onClose}
  oncancel={onClose}
>
  <form class="modal__body" onsubmit={onSubmit}>
    <header class="modal__head">
      <h3 class="modal__title">Draft QC report</h3>
      <p class="modal__sub">
        Against <span class="mono">{woLabel}</span>
      </p>
    </header>

    <label class="field">
      <span class="field__label">Report kind</span>
      <select
        class="field__input"
        value={form.kind}
        onchange={(e) =>
          (form.kind = (e.currentTarget as HTMLSelectElement)
            .value as DraftFormState["kind"])}
      >
        {#each DRAFTABLE_KINDS as k (k.input)}
          <option value={k.input}>{k.label}</option>
        {/each}
      </select>
    </label>

    <label class="field">
      <span class="field__label">Template</span>
      <select
        class="field__input"
        value={form.template}
        onchange={(e) =>
          (form.template = (e.currentTarget as HTMLSelectElement)
            .value as DraftFormState["template"])}
      >
        <option value="">Customer default</option>
        <option value="aben_standard">ÁBEN standard</option>
        <option value="as9102_rev_c">AS9102 Rev C</option>
        <option value="coc_only">CoC only</option>
      </select>
      <span class="field__hint">
        Leave on “Customer default” to use the partner’s configured template.
      </span>
    </label>

    <label class="field">
      <span class="field__label">Notes (optional)</span>
      <textarea
        class="field__input"
        rows="2"
        value={form.notes}
        oninput={(e) =>
          (form.notes = (e.currentTarget as HTMLTextAreaElement).value)}
        placeholder="Internal note — not part of the certified document body."
      ></textarea>
    </label>

    {#if fieldErrors.wo_id}
      <p class="modal__error" role="alert">{fieldErrors.wo_id}</p>
    {/if}
    {#if submitError !== null}
      <p class="modal__error" role="alert">{submitError}</p>
    {/if}

    <footer class="modal__foot">
      <button type="button" class="quiet-button" onclick={onClose}>
        Cancel
      </button>
      <button type="submit" class="primary-button" disabled={submitting}>
        {submitting ? "Drafting…" : "Draft report"}
      </button>
    </footer>
  </form>
</dialog>

<style>
  .modal {
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-md, 8px);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 460px;
    width: 92vw;
  }
  .modal::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }
  .modal__body {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4);
  }
  .modal__head {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .modal__title {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 600;
    color: var(--color-text-strong);
  }
  .modal__sub {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .field__label {
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
  }
  .field__input {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
    border-radius: var(--radius-sm);
    font-family: var(--type-family-body);
  }
  .field__hint {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }
  .modal__error {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    word-break: break-word;
  }
  .modal__foot {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-2);
  }
  .mono {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }
  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    border-radius: var(--radius-sm);
  }
  .quiet-button:hover {
    color: var(--color-text-strong);
  }
  .primary-button {
    padding: var(--space-2) var(--space-4);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }
  .primary-button:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
