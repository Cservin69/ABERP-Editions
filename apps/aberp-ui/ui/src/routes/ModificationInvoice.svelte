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
    type BankAccountSnapshot,
    type Currency,
    type Partner,
  } from "../lib/api";
  import {
    composeModificationBody,
    emptyModificationForm,
    formFromIssuanceInput,
    type ModificationFormState,
  } from "../lib/modification";
  // S174 / PR-174 — reuse Issue's preflight-parse + field-routing
  // helpers so the Modify form gets the same inline-error surface
  // Issue has carried since S91 (ADR-0038). The shape of
  // `validate_invoice_preflight`'s 400 body is identical across the
  // issue and modification HTTP routes (both go through
  // `handle_preflight_response`), so sharing the parser + router is
  // the correct level of reuse — re-implementing them would invite
  // closed-vocab drift.
  import {
    parseInvoicePreflightErrors,
    targetForFieldPath,
    type InvoicePreflightErrorBody,
    type InvoicePreflightErrorItem,
  } from "../lib/issue-invoice";
  import { buyerFieldsFromPartner } from "../lib/partners";
  import PartnerTypeahead from "../lib/PartnerTypeahead.svelte";
  import InvoiceLineFields from "../lib/InvoiceLineFields.svelte";

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
    /** PR-80 / session-102 — base invoice's bank-account snapshot, so
     * the modify form can render a read-only "inherited bank account"
     * affordance. Modification chain children inherit the bank from
     * the base implicitly (the backend's `issue_modification` stamps
     * the base's snapshot onto the new modification's invoice row);
     * the form shows what the inherited bank IS so the operator
     * confirms the right routing before submitting. `null` for
     * CLI-issued bases (no snapshot on the base) or any forward-compat
     * gap; the form falls back to a muted "inherited from base"
     * placeholder in that case. */
    baseBankAccount: BankAccountSnapshot | null;
    /** Invoked with the freshly-issued modification's id when the
     * backend returns 200. The parent uses this to navigate the
     * detail modal to the NEW modification invoice. */
    onAmended: (newInvoiceId: string) => void;
    /** Invoked when the operator closes the modal (ESC / backdrop /
     * Cancel button) without issuing. */
    onClose: () => void;
  }

  let {
    baseInvoiceId,
    baseCurrency,
    baseInvoiceNumber,
    baseBankAccount,
    onAmended,
    onClose,
  }: Props = $props();

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

  // S174 / PR-174 — typed preflight error state. Mirrors
  // `IssueInvoice.svelte`'s posture verbatim (per-field error buckets
  // populated by `routePreflightErrors`, cleared at the top of every
  // `handleSubmit` via `clearPreflightErrors`). The Modify form has
  // no bank picker — chain children inherit the base's bank-account
  // snapshot per ADR-0040 §addendum — so a backend `bankAccountId`
  // preflight error would be unexpected here; route it to the
  // unrouted bucket where the general error block renders it
  // verbatim rather than dropping silently (CLAUDE.md rule 12).
  let preflightErrors: InvoicePreflightErrorBody | null = $state(null);
  let customerErrors: {
    name: InvoicePreflightErrorItem | null;
    taxNumber: InvoicePreflightErrorItem | null;
    address: InvoicePreflightErrorItem | null;
  } = $state({ name: null, taxNumber: null, address: null });
  let linesContainerError: InvoicePreflightErrorItem | null = $state(null);
  let lineErrors: Record<
    number,
    Partial<
      Record<
        "description" | "quantity" | "unitPrice" | "vatRatePercent",
        InvoicePreflightErrorItem
      >
    >
  > = $state({});
  let unroutedPreflightErrors: InvoicePreflightErrorItem[] = $state([]);
  /** PR-54 / session-74 — typeahead-bound buyer-name string. Same
   * decoupling posture as IssueInvoice: typing a search prefix does
   * not commit to the wire body until the operator either picks a
   * saved partner or accepts the typed value as a one-off. */
  let buyerTypeahead = $state("");

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
      buyerTypeahead = form.customerName;
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

  function onPartnerSelect(partner: Partner) {
    const fields = buyerFieldsFromPartner(partner);
    form = {
      ...form,
      // PR-97 / ADR-0048 — overwrite buyer-kind from picked partner.
      customerVatStatus: fields.customerVatStatus,
      // Type-compatibility with `IssueInvoiceFormState`; the
      // modification composer does NOT emit `partnerId` so this
      // value is never observed by the backend chain path.
      customerPartnerId: partner.id,
      customerName: fields.customerName,
      customerTaxNumber: fields.customerTaxNumber,
      // PR-77 / session-101 — auto-fill customer address from the
      // partner record so the modification's `<customerAddress>` body
      // satisfies NAV's `CUSTOMER_DATA_EXPECTED` business rule
      // unconditionally.
      customerCountryCode: fields.customerCountryCode,
      customerPostalCode: fields.customerPostalCode,
      customerCity: fields.customerCity,
      customerStreet: fields.customerStreet,
      // PR-203 / S203 — re-pre-fill from the newly-picked partner. The
      // operator may have edited the override mid-modification; switching
      // partners here resets the override to the new partner's master.
      emailRecipientOverride: fields.emailRecipientOverride,
    };
    buyerTypeahead = partner.display_name;
  }

  function onPartnerOneOff() {
    form = {
      ...form,
      customerName: buyerTypeahead.trim(),
    };
  }

  function addLine() {
    form = {
      ...form,
      lines: [
        ...form.lines,
        {
          description: "",
          // S157 — fresh line seeds quantity "1"; operator can enter any
          // positive decimal.
          quantityInput: "1",
          // PR-88 / session-113 — fresh line seeds with an empty
          // operator-input string. The form's required-attribute
          // forces a value before submit; the parser produces the
          // wire-side minor units at compose time.
          unitPriceInput: "",
          vatRatePercent: 27,
          // PR-82 — fresh line has no buyer note; operator opt-in.
          note: "",
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

  // S174 / PR-174 — route a preflight body's items into the per-
  // field buckets above; mirror of `IssueInvoice.svelte`'s
  // `routePreflightErrors`. The Modify form has no bank picker so
  // `bankAccountId` targets fall through to the unrouted bucket
  // (the general error block surfaces them verbatim — fail loud).
  function routePreflightErrors(body: InvoicePreflightErrorBody) {
    customerErrors = { name: null, taxNumber: null, address: null };
    linesContainerError = null;
    lineErrors = {};
    unroutedPreflightErrors = [];
    for (const item of body.errors) {
      const target = targetForFieldPath(item.field_path);
      if (!target) {
        unroutedPreflightErrors.push(item);
        continue;
      }
      switch (target.kind) {
        case "customer":
          customerErrors[target.field] = item;
          break;
        case "lines":
          linesContainerError = item;
          break;
        case "bankAccountId":
          // Modify has no bank picker — chain inherits per
          // ADR-0040 §addendum. Surface as unrouted so the operator
          // still sees the message rather than a silent drop.
          unroutedPreflightErrors.push(item);
          break;
        case "line": {
          const bucket = lineErrors[target.lineIndex] ?? {};
          bucket[target.field] = item;
          lineErrors[target.lineIndex] = bucket;
          break;
        }
      }
    }
  }

  function clearPreflightErrors() {
    preflightErrors = null;
    customerErrors = { name: null, taxNumber: null, address: null };
    linesContainerError = null;
    lineErrors = {};
    unroutedPreflightErrors = [];
  }

  async function handleSubmit(event: Event) {
    event.preventDefault();
    if (baseInvoiceId === null) return;
    modalState = "submitting";
    submitError = null;
    clearPreflightErrors();
    try {
      const body = composeModificationBody(form);
      const response = await amendInvoiceModification(baseInvoiceId, body);
      modalState = "prefilled";
      onAmended(response.invoice_id);
    } catch (err: unknown) {
      modalState = "error";
      const raw = err instanceof Error ? err.message : String(err);
      // S174 — try the preflight 400 shape first (same posture as
      // IssueInvoice's catch). On match, populate the per-field
      // routing so each invalid input gets its own inline message;
      // otherwise fall back to the raw string in the general error
      // block below.
      const preflight = parseInvoicePreflightErrors(raw);
      if (preflight) {
        preflightErrors = preflight;
        routePreflightErrors(preflight);
      }
      submitError = raw;
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
      {#if preflightErrors}
        <!-- S174 — typed preflight summary + unrouted catch-all.
             Mirrors `IssueInvoice.svelte`'s top-of-form error surface
             so the Modify form gives operators the same recovery
             affordance for ADR-0038 preflight failures. -->
        <div class="error error-typed" role="alert" data-testid="modification-preflight-summary">
          <p class="error-summary">
            {preflightErrors.errors.length} validation
            {preflightErrors.errors.length === 1 ? "issue" : "issues"} —
            fix the highlighted fields below and re-submit.
          </p>
          {#each unroutedPreflightErrors as item (item.field_path + item.kind)}
            <p class="error-path">
              {item.field_path}: {item.message_hu}
            </p>
            <p class="error-hint">{item.message_en}</p>
          {/each}
        </div>
      {:else}
        <p class="error" role="alert">{submitError}</p>
      {/if}
    {/if}

    <fieldset disabled={modalState === "prefilling"}>
      <legend>Buyer</legend>
      <label>
        <span>Search saved partners</span>
        <PartnerTypeahead
          bind:value={buyerTypeahead}
          onSelect={onPartnerSelect}
          onUseAsOneOff={onPartnerOneOff}
          placeholder="Type 3+ characters to search…"
          ariaLabel="Search saved partners"
        />
      </label>
      <label>
        <span>Name</span>
        <input
          type="text"
          bind:value={form.customerName}
          required
          class:input-invalid={customerErrors.name !== null}
          aria-invalid={customerErrors.name !== null}
          data-testid="mod-customer-name-input"
        />
        {#if customerErrors.name}
          <p
            class="inline-error"
            data-testid="mod-customer-name-error"
            data-kind={customerErrors.name.kind}
          >
            <span class="inline-error-hu">{customerErrors.name.message_hu}</span>
            <span class="inline-error-en">{customerErrors.name.message_en}</span>
          </p>
        {/if}
      </label>
      <label>
        <span>ADÓSZÁM</span>
        <input
          type="text"
          bind:value={form.customerTaxNumber}
          required
          placeholder="87654321-2-13"
          class:input-invalid={customerErrors.taxNumber !== null}
          aria-invalid={customerErrors.taxNumber !== null}
          data-testid="mod-customer-tax-input"
        />
        {#if customerErrors.taxNumber}
          <p
            class="inline-error"
            data-testid="mod-customer-tax-error"
            data-kind={customerErrors.taxNumber.kind}
          >
            <span class="inline-error-hu">{customerErrors.taxNumber.message_hu}</span>
            <span class="inline-error-en">{customerErrors.taxNumber.message_en}</span>
          </p>
        {/if}
      </label>
      <!-- PR-77 / session-101 — customer address quartet. Same NAV
           `CUSTOMER_DATA_EXPECTED` rule applies to modifications;
           inherits from base side-store when present, partner combobox
           overrides when picked. S174 — the address-shape preflight
           gate (`CustomerAddressMissing`, §169) renders a single
           inline-error block under the quartet, mirroring IssueInvoice. -->
      <label>
        <span>Country code (ISO 3166-1)</span>
        <input
          type="text"
          bind:value={form.customerCountryCode}
          required
          placeholder="HU"
          maxlength="2"
          data-testid="mod-customer-country-input"
        />
      </label>
      <label>
        <span>Postal code</span>
        <input
          type="text"
          bind:value={form.customerPostalCode}
          required
          placeholder="1052"
          data-testid="mod-customer-postal-input"
        />
      </label>
      <label>
        <span>City</span>
        <input
          type="text"
          bind:value={form.customerCity}
          required
          placeholder="Budapest"
          data-testid="mod-customer-city-input"
        />
      </label>
      <label>
        <span>Street</span>
        <input
          type="text"
          bind:value={form.customerStreet}
          required
          placeholder="Váci utca 19."
          data-testid="mod-customer-street-input"
        />
      </label>
      {#if customerErrors.address}
        <p
          class="inline-error"
          data-testid="mod-customer-address-error"
          data-kind={customerErrors.address.kind}
        >
          <span class="inline-error-hu">{customerErrors.address.message_hu}</span>
          <span class="inline-error-en">{customerErrors.address.message_en}</span>
        </p>
      {/if}
    </fieldset>

    <fieldset disabled={modalState === "prefilling"}>
      <legend>Számlalánc / Chain</legend>
      <label>
        <span>Pénznem (örökölt — locked to base)</span>
        <select bind:value={form.currency} disabled>
          <option value={form.currency}>{form.currency}</option>
        </select>
      </label>
      <label>
        <span>Módosítás dátuma / Modification date</span>
        <input
          type="date"
          bind:value={form.modificationDate}
          required
        />
      </label>
      <!-- PR-80 / session-102 — inherited bank readout. Modification
           chain children inherit the bank-account snapshot from the
           base (backend's `issue_modification` stamps the base's
           snapshot onto the new invoice row); the form surfaces what
           the inherited bank IS so the operator confirms the routing
           before submitting. Display-only — no picker, because the
           inheritance is the rule (ADR-0040 §addendum). -->
      <div class="inherited-bank" data-testid="modification-inherited-bank">
        <span class="inherited-bank-label">
          Örökölt bankszámla / Inherited bank account
        </span>
        {#if baseBankAccount === null}
          <span class="inherited-bank-empty mono">
            — (alap számlán nincs banki adat — base has no bank snapshot)
          </span>
        {:else}
          <div class="inherited-bank-grid mono">
            <span>{baseBankAccount.bank_name}</span>
            <span>{baseBankAccount.account_number}</span>
            <span>SWIFT/BIC: {baseBankAccount.swift_bic}</span>
          </div>
        {/if}
      </div>
    </fieldset>

    <fieldset disabled={modalState === "prefilling"}>
      <legend>Corrected line items (full-replace per ADR-0024 §4)</legend>
      {#if linesContainerError}
        <p
          class="inline-error"
          data-testid="mod-lines-container-error"
          data-kind={linesContainerError.kind}
        >
          <span class="inline-error-hu">{linesContainerError.message_hu}</span>
          <span class="inline-error-en">{linesContainerError.message_en}</span>
        </p>
      {/if}
      <!-- S174 / PR-174 — line rows delegate to the shared
           `<InvoiceLineFields>` component. Pre-S174 the line markup was
           duplicated verbatim against IssueInvoice's basic inputs; the
           extraction lifts the shared shape AND wires the per-line
           preflight error block uniformly. Issue's `.line` div stays
           on its hardened in-template wiring this round (product
           combobox + per-line notes + currency-mismatch chip) — see
           the component's header for the migration rationale. -->
      {#each form.lines as line, index (index)}
        <InvoiceLineFields
          bind:description={line.description}
          bind:quantityInput={line.quantityInput}
          bind:unitPriceInput={line.unitPriceInput}
          bind:vatRatePercent={line.vatRatePercent}
          currency={form.currency}
          {index}
          removable={form.lines.length > 1}
          onRemove={() => removeLine(index)}
          errors={lineErrors[index]}
          testidPrefix="mod-line"
        />
      {/each}
      <button type="button" class="quiet-button" onclick={addLine}>
        + Add line
      </button>
    </fieldset>

    <!-- PR-203 / S203 — per-modification email recipient override. Pre-
         filled from the base's stored value via `formFromIssuanceInput`;
         editable per-modification (the modification's own
         `invoice.email_recipient_override` row carries the operator's
         edit independent of the base). NEVER writes back to the partner
         master record. -->
    <fieldset disabled={modalState === "prefilling"}>
      <legend>Email-címzett(ek) / Email recipient(s)</legend>
      <label>
        <input
          type="text"
          bind:value={form.emailRecipientOverride}
          data-testid="mod-email-recipient-override-input"
          autocomplete="off"
          placeholder="pl. vevo@example.com, masik@example.com"
        />
        <span class="invoice-note-hint">
          Vesszővel elválasztva. Egyszeri — nem mentődik a partnerhez. /
          Comma-separated. One-off per modification; never saved back to
          the partner record.
        </span>
      </label>
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

  /* S174 / PR-174 — `.line` / `.line-remove` / `label.narrow` / `label.wide`
   * styles moved to `lib/InvoiceLineFields.svelte` (Svelte component-
   * scoped CSS) so the line-row layout lives next to the markup it
   * styles. `input[type="number"]` only existed on the per-line VAT
   * input which now lives in the shared component too. The dialog's
   * remaining inputs (text + date + select) still need the base
   * input chrome below. */

  input[type="text"],
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

  /* S174 / PR-174 — preflight inline-error chrome for the buyer
   * fieldset + lines-container error. Mirrors IssueInvoice's
   * `.inline-error` / `.input-invalid` selectors so the operator
   * sees the same negative-signal styling across both forms. */
  input.input-invalid {
    border-color: var(--color-signal-negative);
    outline-color: var(--color-signal-negative);
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

  /* S174 — typed preflight summary block. Same chrome IssueInvoice
   * uses (`.error-typed` / `.error-summary` / `.error-hint` /
   * `.error-path`) so the unrouted catch-all paths render
   * consistently across the two forms. */
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

  /* PR-80 / session-102 — inherited-bank readout. Display-only — no
   * input affordance because the inheritance is the rule. Same dt/dd
   * label convention as the rest of the form's fieldsets. */
  .inherited-bank {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
    flex: 1 1 auto;
  }

  .inherited-bank-label {
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .inherited-bank-grid {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: var(--space-1) var(--space-2);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
  }

  .inherited-bank-empty {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
    padding: var(--space-1) var(--space-2);
    background: var(--color-surface-raised);
    border: 1px dashed var(--color-surface-divider);
    border-radius: var(--radius-sm);
  }
</style>
