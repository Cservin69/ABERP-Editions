<script lang="ts">
  // PR-47β / session-65 — Modification (Amend invoice) modal.
  //
  // The SECOND mutation-route surface on the SPA (PR-47α landed the
  // first via the storno button); modification is operator-edited
  // unlike storno (storno reuses the base's content verbatim).
  // Mirrors `IssueInvoice.svelte`'s posture (A157, no toast; inline
  // error) with three divergences per ADR-0024:
  //
  //   1. Currency dropdown is LOCKED to the base invoice's currency
  //      per ADR-0037 §4 invariant C6 (chain children inherit; rate
  //      metadata is frozen at base issuance time). The backend
  //      additionally enforces a 400 if the body's currency differs;
  //      the form's <select disabled> is the first line of defence.
  //
  //   2. A `modificationDate` field carries the operator-supplied
  //      `YYYY-MM-DD` per ADR-0024 §1 (frozen on the audit payload;
  //      no silent today-default — though the form pre-fills today
  //      as a sensible starting value, the operator is free to
  //      overwrite).
  //
  //   3. The form opens pre-filled from the base invoice's side-
  //      stored `<ULID>.input.json` (PR-47α / A174) so the operator
  //      edits in place. CLI-issued invoices (no side-store) fall
  //      back to an empty form with an explanatory banner.
  //
  // On submit:
  //   1. Compose the wire body via `composeModificationBody(form)`.
  //   2. POST via `amendInvoiceModification(invoiceId, body)`.
  //   3. On success, invoke `onAmended(invoice_id)` so the parent
  //      navigates the detail modal open on the NEW modification
  //      invoice (the operator's regulatory record is the chain
  //      child, not the base they amended).
  //   4. On failure, render the error string inline (no toast).

  import {
    amendInvoiceModification,
    getIssuanceInput,
    type Currency,
  } from "../lib/api";
  import {
    composeModificationBody,
    emptyModificationForm,
    formFromIssuanceInput,
    type ModificationFormState,
  } from "../lib/modification";

  interface Props {
    /** The base invoice id this modification references. `null`
     * means the modal is closed; setting to a string opens the
     * modal and triggers the pre-fill fetch. */
    baseInvoiceId: string | null;
    /** The base invoice's currency per ADR-0037 §4 invariant C6.
     * Locked into the form's currency field; never overridable from
     * the modal's UI. Read by the parent from the same
     * `InvoiceDetail` it already has open. */
    baseCurrency: Currency | null;
    /** Operator-readable identifier of the base (e.g.
     * "INV-default/00013") — surfaced in the banner so the operator
     * confirms they're modifying the right invoice. */
    baseInvoiceNumber: string | null;
    /** Invoked with the freshly-issued modification's id when the
     * backend returns 200. The parent uses this to navigate the
     * detail modal to the NEW modification invoice. */
    onAmended: (newInvoiceId: string) => void;
    /** Invoked when the operator closes the modal (ESC / backdrop /
     * Cancel button) without issuing. */
    onClose: () => void;
  }

  let { baseInvoiceId, baseCurrency, baseInvoiceNumber, onAmended, onClose }:
    Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: ModificationFormState = $state(emptyModificationForm("HUF"));
  // `prefilling` while the side-stored input.json fetch is in flight;
  // `prefilled` once successfully loaded (or fallback if 404).
  // `submitting` / `error` mirror the IssueInvoice posture.
  let modalState:
    | "idle"
    | "prefilling"
    | "prefilled"
    | "submitting"
    | "error" = $state("idle");
  let submitError: string | null = $state(null);
  let prefillFallback: string | null = $state(null);

  // Drive the dialog open/close lifecycle from the `baseInvoiceId`
  // prop. Opening: showModal() + kick off the pre-fill fetch.
  // Closing: close() if open.
  $effect(() => {
    if (!dialogEl) return;
    if (baseInvoiceId !== null && baseCurrency !== null) {
      if (!dialogEl.open) {
        dialogEl.showModal();
        // Reset state every time the modal opens for a fresh base.
        modalState = "prefilling";
        submitError = null;
        prefillFallback = null;
        form = emptyModificationForm(baseCurrency);
        void prefillFromBase(baseInvoiceId, baseCurrency);
      }
    } else {
      if (dialogEl.open) dialogEl.close();
    }
  });

  async function prefillFromBase(invoiceId: string, currency: Currency) {
    try {
      const input = await getIssuanceInput(invoiceId);
      // Defence in depth — `getIssuanceInput` returns the body shape
      // verbatim; we still source currency from the billing row
      // (passed in as `currency`) per the C6 source-of-truth posture.
      form = formFromIssuanceInput(input, currency);
      modalState = "prefilled";
    } catch (err: unknown) {
      // 404 (CLI-issued or pre-PR-47α SPA-issued) lands here as a
      // rejected promise per the forward_get error-string posture.
      // The form stays at `emptyModificationForm(currency)` (already
      // initialised above); surface a banner explaining the fallback
      // per CLAUDE.md rule 12 so the operator is not silently confused
      // about empty fields.
      prefillFallback =
        err instanceof Error ? err.message : String(err);
      modalState = "prefilled";
    }
  }

  function addLine() {
    form = {
      ...form,
      lines: [
        ...form.lines,
        {
          description: "",
          quantity: 1,
          unitPriceMinor: 0,
          vatRatePercent: 27,
        },
      ],
    };
  }

  function removeLine(index: number) {
    if (form.lines.length <= 1) return;
    form = {
      ...form,
      lines: form.lines.filter((_, i) => i !== index),
    };
  }

  async function handleSubmit(event: Event) {
    event.preventDefault();
    if (baseInvoiceId === null) return;
    modalState = "submitting";
    submitError = null;
    try {
      const body = composeModificationBody(form);
      const response = await amendInvoiceModification(baseInvoiceId, body);
      modalState = "prefilled";
      onAmended(response.invoice_id);
    } catch (err: unknown) {
      modalState = "error";
      submitError = err instanceof Error ? err.message : String(err);
    }
  }

  function handleDialogClose() {
    onClose();
  }

  function handleDialogClick(event: MouseEvent) {
    if (event.target === dialogEl) {
      dialogEl?.close();
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="modification"
  onclose={handleDialogClose}
  onclick={handleDialogClick}
  aria-label="Amend invoice (modification)"
>
  <form class="modification-frame" onsubmit={handleSubmit}>
    <header class="modification-head">
      <h2>Amend invoice</h2>
      <button
        type="button"
        class="quiet-button"
        onclick={() => dialogEl?.close()}
        aria-label="Cancel modification"
      >
        Cancel
      </button>
    </header>

    {#if baseInvoiceNumber}
      <p class="banner" role="status">
        This will issue a modification invoice referencing
        <strong>{baseInvoiceNumber}</strong>. The new invoice will inherit
        the same currency
        ({baseCurrency}) and exchange rate per ADR-0037 §4 invariant C6.
      </p>
    {/if}

    {#if prefillFallback}
      <p class="hint" role="note">
        Pre-fill unavailable for this base ({prefillFallback}). Fill the
        form manually with the corrected invoice content.
      </p>
    {/if}

    {#if modalState === "prefilling"}
      <p class="muted">Loading base invoice content…</p>
    {/if}

    {#if modalState === "error" && submitError}
      <p class="error" role="alert">{submitError}</p>
    {/if}

    <fieldset disabled={modalState === "prefilling"}>
      <legend>Supplier</legend>
      <label>
        <span>Name</span>
        <input type="text" bind:value={form.supplierName} required />
      </label>
      <label>
        <span>ADÓSZÁM</span>
        <input
          type="text"
          bind:value={form.supplierTaxNumber}
          required
          placeholder="12345678-1-42"
        />
      </label>
      <div class="row">
        <label class="narrow">
          <span>Country</span>
          <input
            type="text"
            bind:value={form.supplierCountryCode}
            maxlength="2"
            required
          />
        </label>
        <label class="narrow">
          <span>Postal code</span>
          <input type="text" bind:value={form.supplierPostalCode} required />
        </label>
        <label>
          <span>City</span>
          <input type="text" bind:value={form.supplierCity} required />
        </label>
      </div>
      <label>
        <span>Street</span>
        <input type="text" bind:value={form.supplierStreet} required />
      </label>
    </fieldset>

    <fieldset disabled={modalState === "prefilling"}>
      <legend>Buyer</legend>
      <label>
        <span>Name</span>
        <input type="text" bind:value={form.customerName} required />
      </label>
      <label>
        <span>ADÓSZÁM</span>
        <input
          type="text"
          bind:value={form.customerTaxNumber}
          required
          placeholder="87654321-2-13"
        />
      </label>
    </fieldset>

    <fieldset disabled={modalState === "prefilling"}>
      <legend>Chain</legend>
      <label>
        <span>Currency (locked to base)</span>
        <select bind:value={form.currency} disabled>
          <option value={form.currency}>{form.currency}</option>
        </select>
      </label>
      <label>
        <span>Modification date</span>
        <input
          type="date"
          bind:value={form.modificationDate}
          required
        />
      </label>
    </fieldset>

    <fieldset disabled={modalState === "prefilling"}>
      <legend>Corrected line items (full-replace per ADR-0024 §4)</legend>
      {#each form.lines as line, index (index)}
        <div class="line">
          <label class="wide">
            <span>Description</span>
            <input type="text" bind:value={line.description} required />
          </label>
          <label class="narrow">
            <span>Qty</span>
            <input
              type="number"
              min="1"
              step="1"
              bind:value={line.quantity}
              required
            />
          </label>
          <label class="narrow">
            <span>Unit price</span>
            <input
              type="number"
              min="0"
              step="1"
              bind:value={line.unitPriceMinor}
              required
            />
          </label>
          <label class="narrow">
            <span>VAT %</span>
            <input
              type="number"
              min="0"
              max="100"
              step="1"
              bind:value={line.vatRatePercent}
              required
            />
          </label>
          <button
            type="button"
            class="quiet-button line-remove"
            onclick={() => removeLine(index)}
            disabled={form.lines.length <= 1}
            aria-label={`Remove line ${index + 1}`}
            title={form.lines.length <= 1
              ? "At least one line is required"
              : `Remove line ${index + 1}`}
          >
            ✕
          </button>
        </div>
      {/each}
      <button type="button" class="quiet-button" onclick={addLine}>
        + Add line
      </button>
    </fieldset>

    <footer class="modification-foot">
      <button
        type="submit"
        class="quiet-button primary"
        disabled={modalState === "prefilling" || modalState === "submitting"}
      >
        {#if modalState === "submitting"}
          <span aria-hidden="true">…</span> Issuing modification
        {:else}
          Issue modification
        {/if}
      </button>
    </footer>
  </form>
</dialog>

<style>
  dialog.modification {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 720px;
    overflow: hidden;
  }

  dialog.modification::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .modification-frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    max-height: 90vh;
    overflow: auto;
    padding: var(--space-4) var(--space-5);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .modification-head {
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

  fieldset {
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  legend {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
    padding: 0 var(--space-2);
  }

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

  .row {
    display: flex;
    gap: var(--space-2);
  }

  .line {
    display: flex;
    gap: var(--space-2);
    align-items: flex-end;
  }

  input[type="text"],
  input[type="number"],
  input[type="date"],
  select {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  input:focus-visible,
  select:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 1px;
    border-color: var(--color-text-muted);
  }

  select:disabled {
    color: var(--color-text-muted);
    cursor: not-allowed;
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

  .quiet-button.primary {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .line-remove {
    flex: 0 0 auto;
    align-self: flex-end;
  }

  .modification-foot {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .banner {
    margin: 0;
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: var(--type-line-normal);
  }

  .banner strong {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }

  .muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
    margin: 0;
  }

  .hint {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-style: italic;
  }

  .error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
  }
</style>
