<script lang="ts">
  // PR-44ζ / session-59 — Issue-invoice modal.
  //
  // The first MUTATION-route surface on the SPA (every prior screen
  // is read-only per ADR-0021 §Part B). Wraps the new
  // `POST /invoices/issue` route behind a native <dialog> modal,
  // matching `InvoiceDetail.svelte`'s posture per A157 (no toast
  // component; inline error rendering).
  //
  // Form fields (per the session-59 brief, surgical subset):
  //   - Supplier (name, taxNumber, address: country/postal/city/street)
  //   - Customer (name, taxNumber)
  //   - Currency (HUF / EUR dropdown)
  //   - Lines (1+) with description, quantity, unitPrice, vatRatePercent
  //
  // Deliberately NOT on the form (per CLAUDE.md rule 3, surgical):
  //   - Fulfillment date / payment due date / payment method — the
  //     backend's `nav_xml.rs` hardcodes these to issue_date and
  //     "TRANSFER" today; exposing form fields the backend silently
  //     ignores would violate rule 12. Future PR-44ζ.1 widens the
  //     input schema + render to support them.
  //   - Series picker — backend defaults to "INV-default"; the
  //     wire-shape supports overriding via `series?: string` but
  //     the form does not expose it on the first cut.
  //   - Customer address — backend's CustomerJson does not carry an
  //     address field today; widening that surface is named-deferred.
  //
  // On submit:
  //   1. Compose the wire body via `composeIssueInvoiceBody(form)`.
  //   2. POST via `issueInvoice(body)` Tauri command.
  //   3. On success, invoke `onIssued(invoice_id)` so the parent
  //      navigates the detail modal open on the just-issued invoice.
  //   4. On failure, render the error string inline (no toast).

  import {
    issueInvoice,
    type Currency,
  } from "../lib/api";
  import {
    composeIssueInvoiceBody,
    emptyForm,
    emptyLine,
    parseMissingSellerConfigError,
    type IssueInvoiceFormState,
    type MissingSellerConfigError,
  } from "../lib/issue-invoice";

  interface Props {
    /** Whether the modal is open. The parent toggles by reassigning
     * this boolean prop; `null`-vs-string would mirror the detail
     * modal's posture but the issue modal has no per-invocation
     * payload to carry, so a boolean is the simpler shape. */
    open: boolean;
    /** Invoked with the freshly-issued invoice id when the backend
     * returns 200. The parent uses this to navigate the detail
     * modal open at the just-issued invoice. */
    onIssued: (invoiceId: string) => void;
    /** Invoked when the operator closes the modal (ESC / backdrop /
     * Cancel button) without issuing. */
    onClose: () => void;
  }

  let { open, onIssued, onClose }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: IssueInvoiceFormState = $state(emptyForm());
  let submitState: "idle" | "submitting" | "error" = $state("idle");
  let submitError: string | null = $state(null);
  /** PR-50 / session-70 — when the backend's `400` body carries the
   * `missing_seller_config` discriminant, hold the typed shape so the
   * template can render the operator-actionable `config_path` +
   * `sample_path` hints instead of just the raw message. `null` for
   * every other error class (network, 500, plain 400). */
  let missingSellerConfig: MissingSellerConfigError | null = $state(null);

  // Sync the native <dialog>'s open state with the `open` prop.
  // `showModal()` and `close()` are imperative; the prop is the
  // declarative source of truth.
  $effect(() => {
    if (!dialogEl) return;
    if (open) {
      if (!dialogEl.open) dialogEl.showModal();
    } else {
      if (dialogEl.open) dialogEl.close();
    }
  });

  function resetForm() {
    form = emptyForm();
    submitState = "idle";
    submitError = null;
    missingSellerConfig = null;
  }

  function addLine() {
    form = { ...form, lines: [...form.lines, emptyLine()] };
  }

  function removeLine(index: number) {
    // Refuse to delete the last line — the form must always have
    // one line for the backend's `at least one line item is required`
    // pre-validation to pass. The button is disabled in the markup
    // when `form.lines.length === 1` so the operator gets a visual
    // cue too.
    if (form.lines.length <= 1) return;
    form = {
      ...form,
      lines: form.lines.filter((_, i) => i !== index),
    };
  }

  async function handleSubmit(event: Event) {
    event.preventDefault();
    submitState = "submitting";
    submitError = null;
    missingSellerConfig = null;
    try {
      const body = composeIssueInvoiceBody(form);
      const response = await issueInvoice(body);
      submitState = "idle";
      // Reset the form so the next opening starts fresh; the parent
      // owns the open/close lifecycle.
      resetForm();
      onIssued(response.invoice_id);
    } catch (err: unknown) {
      submitState = "error";
      const raw = err instanceof Error ? err.message : String(err);
      // PR-50 / session-70 — when the backend's 400 body carries the
      // typed `missing_seller_config` discriminant, populate the
      // structured state so the template renders the
      // config_path + sample_path hints. Otherwise fall back to
      // displaying the raw error string verbatim.
      missingSellerConfig = parseMissingSellerConfigError(raw);
      submitError = raw;
    }
  }

  function handleDialogClose() {
    // Reset form on close so an aborted issuance does not leak
    // operator-typed values into the next opening — the same
    // posture InvoiceDetail.svelte uses for its per-invocation
    // state (download error, expanded payloads).
    resetForm();
    onClose();
  }

  function handleDialogClick(event: MouseEvent) {
    if (event.target === dialogEl) {
      dialogEl?.close();
    }
  }

  // Currency dropdown options. The backend's `Currency` Deserialize
  // accepts exactly two strings per `rename_all = "UPPERCASE"`;
  // mirror that closed vocab here so a future widening (ADR-0037 §5)
  // surfaces at both ends.
  const CURRENCY_OPTIONS: Currency[] = ["HUF", "EUR"];
</script>

<dialog
  bind:this={dialogEl}
  class="issue"
  onclose={handleDialogClose}
  onclick={handleDialogClick}
  aria-label="Issue new invoice"
>
  <form class="issue-frame" onsubmit={handleSubmit}>
    <header class="issue-head">
      <h2>New invoice</h2>
      <button
        type="button"
        class="quiet-button"
        onclick={() => dialogEl?.close()}
        aria-label="Cancel issuance"
      >
        Cancel
      </button>
    </header>

    {#if submitState === "error" && submitError}
      {#if missingSellerConfig}
        <div class="error error-typed" role="alert">
          <p class="error-summary">{missingSellerConfig.message}</p>
          <p class="error-hint">
            Per-tenant config home (PR-51 will route this through the
            wizard):
          </p>
          <p class="error-path">{missingSellerConfig.config_path}</p>
          <p class="error-hint">Template to copy from:</p>
          <p class="error-path">{missingSellerConfig.sample_path}</p>
        </div>
      {:else}
        <p class="error" role="alert">{submitError}</p>
      {/if}
    {/if}

    <fieldset>
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

    <fieldset>
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

    <fieldset>
      <legend>Currency</legend>
      <label>
        <span>Currency</span>
        <select bind:value={form.currency}>
          {#each CURRENCY_OPTIONS as option (option)}
            <option value={option}>{option}</option>
          {/each}
        </select>
      </label>
      {#if form.currency === "EUR"}
        <p class="hint">
          The MNB exchange rate will be fetched at issuance per
          ADR-0037 §2.b (with D-1 walk-back).
        </p>
      {/if}
    </fieldset>

    <fieldset>
      <legend>Line items</legend>
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

    <footer class="issue-foot">
      <button
        type="submit"
        class="quiet-button primary"
        disabled={submitState === "submitting"}
      >
        {#if submitState === "submitting"}
          <span aria-hidden="true">…</span> Issuing
        {:else}
          Issue invoice
        {/if}
      </button>
    </footer>
  </form>
</dialog>

<style>
  dialog.issue {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 720px;
    overflow: hidden;
  }

  dialog.issue::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .issue-frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    max-height: 90vh;
    overflow: auto;
    padding: var(--space-4) var(--space-5);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .issue-head {
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

  /* `.primary` is a quiet emphasis — the dense ADR-0017 aesthetic
   * keeps the chrome quiet; the primary button is just slightly
   * stronger than `.quiet-button` to mark "this is the action". */
  .quiet-button.primary {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .line-remove {
    flex: 0 0 auto;
    align-self: flex-end;
  }

  .issue-foot {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
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

  /* PR-50 / session-70 — typed `missing_seller_config` block. Same
   * negative-signal colour as the plain inline error, with extra
   * structure so the config_path + sample_path hints render as
   * monospaced "you can copy this" lines. */
  .error-typed {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .error-summary {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  .error-hint {
    color: var(--color-text-secondary);
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    margin: 0;
  }

  .error-path {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: 0;
    word-break: break-all;
  }
</style>
