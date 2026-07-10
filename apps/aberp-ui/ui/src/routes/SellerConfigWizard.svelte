<script lang="ts">
  // PR-51 / session-71 — second-run seller-config wizard. Renders
  // when the Tauri shell reports BootStatus === "needs-seller-config"
  // (i.e. NAV credentials are populated but
  // `~/.aberp/<tenant>/seller.toml` is missing or identity-
  // incomplete). Eleven form fields (3 identity + optional EU VAT + 4
  // address + 4 optional bank) → POST /api/setup-seller-info via the
  // matching Tauri command → backend writes the file atomically +
  // flips its boot state to Ready → SPA re-mounts the normal app on
  // the next getBootStatus poll. Zero terminal interaction needed
  // (the milestone PR-51 closes).
  //
  // The composer + validator + typed-error parser live in
  // `seller-config.ts` so vitest can pin the wire shape without
  // mounting this component (component-test runner is named-deferred
  // per CLAUDE.md rule 2; composer-pin pattern is A156 / A161 / A163
  // precedent + extended in PR-46α's setup-credentials.ts).

  import { createSellerBank, setupSellerInfo } from "../lib/api";
  import {
    composeSellerConfigBody,
    DEFAULT_SELLER_CONFIG_FORM,
    parseSetupSellerInfoErrorBody,
    validateSellerConfig,
    type SellerConfigForm,
  } from "../lib/seller-config";
  import {
    composeSellerBankInputs,
    emptySellerBankForm,
    emptyWizardBankRows,
    parseSellerBankValidationError,
    validateWizardBankRows,
    type WizardBankRow,
  } from "../lib/seller-banks";

  let form: SellerConfigForm = $state({ ...DEFAULT_SELLER_CONFIG_FORM });
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  // PR-72 / session-94 — multi-bank-row state for the wizard's bank
  // step. Replaces the legacy single-bank fields on the seller-config
  // form (the operator can now add HUF + EUR + N more rows at first
  // run; the simplest valid setup is one HUF row marked default).
  let bankRows: WizardBankRow[] = $state(emptyWizardBankRows());
  let nextRowSeq = $state(2);
  let bankSubmitError: string | null = $state(null);
  let bankRowErrors: Record<string, Record<string, string>> = $state({});

  let validation = $derived(validateSellerConfig(form));
  let bankValidation = $derived(validateWizardBankRows(bankRows));

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    fieldErrors = {};
    bankSubmitError = null;
    bankRowErrors = {};
    if (!validation.ok || !bankValidation.ok) {
      return;
    }
    submitting = true;
    try {
      // PR-72 / session-94 — two-phase write at wizard close:
      //   1. POST identity (the legacy single-bank fields on this
      //      request go to /api/setup-seller-info empty; PR-71 keeps
      //      them live for the PDF + NAV body until PR-D swaps them).
      //   2. POST each bank row in sequence to /api/seller/banks.
      // Identity flip transitions the backend to Ready first; if a
      // bank-row POST then fails, the operator lands in the normal
      // app and can fix it via Tenant Settings → Bank accounts.
      const identityBody = composeSellerConfigBody({
        ...form,
        bankAccountNumber: "",
        iban: "",
        bankName: "",
        swiftBic: "",
      });
      await setupSellerInfo(identityBody);
      for (const row of bankRows) {
        try {
          await createSellerBank(composeSellerBankInputs(row));
        } catch (bankErr: unknown) {
          const message =
            bankErr instanceof Error ? bankErr.message : String(bankErr);
          const typed = parseSellerBankValidationError(message);
          if (typed !== null) {
            const fieldMap: Record<string, string> = {};
            for (const f of typed.fields) {
              fieldMap[f.field] = f.message;
            }
            bankRowErrors[row.rowKey] = fieldMap;
            bankSubmitError =
              "Identity saved, but one or more bank rows failed validation. " +
              "Fix the inline errors below; rows already saved are not re-sent.";
          } else {
            bankSubmitError = `Identity saved, but a bank row failed: ${message}`;
          }
          throw bankErr;
        }
      }
      // On success the Tauri shell has flipped the boot-state mirror
      // to Ready (via mark_post_setup_state). App.svelte's poll
      // picks it up within ~300ms and re-renders against the normal
      // app.
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      // PR-51 / session-71 — try parsing the typed 400 body for
      // field-level inline errors first; fall back to the raw banner
      // for non-validation failures (500 atomic-write errors, etc.).
      const typed = parseSetupSellerInfoErrorBody(message);
      if (typed !== null) {
        const next: Record<string, string> = {};
        for (const f of typed.fields) {
          next[f.field] = f.message;
        }
        fieldErrors = next;
        submitError = "Some fields need attention — see the inline messages.";
      } else if (submitError === null && bankSubmitError === null) {
        submitError = message;
      }
    } finally {
      submitting = false;
    }
  }

  function fieldError(name: string, clientSide: string | null): string | null {
    // Backend-reported field errors take precedence over client-side
    // ones so the operator sees the same message the server saw (the
    // backend is the authoritative validator per A157).
    if (fieldErrors[name] !== undefined) {
      return fieldErrors[name];
    }
    return clientSide;
  }

  function addBankRow() {
    const rowKey = `row-${nextRowSeq}`;
    nextRowSeq += 1;
    bankRows = [
      ...bankRows,
      { ...emptySellerBankForm(), rowKey },
    ];
  }

  function removeBankRow(rowKey: string) {
    bankRows = bankRows.filter((r) => r.rowKey !== rowKey);
    delete bankRowErrors[rowKey];
  }

  function bankRowFieldError(rowKey: string, fieldName: string): string | null {
    const map = bankRowErrors[rowKey];
    if (map && map[fieldName]) return map[fieldName];
    return null;
  }
</script>

<section class="wizard" role="form" aria-labelledby="wizard-title">
  <h2 id="wizard-title" class="wizard__title">Seller information</h2>
  <p class="wizard__lede">
    ABERP needs your company's identity for the supplier block on every
    NAV invoice. These values are stored in a per-tenant config file
    (<code>~/.aberp/&lt;tenant&gt;/seller.toml</code>); you can edit it
    later from the settings screen.
  </p>

  <form onsubmit={onSubmit} class="wizard__form">
    <fieldset disabled={submitting} class="wizard__fieldset">
      <h3 class="wizard__section">Identity</h3>

      <label class="field">
        <span class="field__label">Legal name</span>
        <input
          class="field__input"
          type="text"
          autocomplete="organization"
          bind:value={form.legalName}
          aria-invalid={fieldError("legalName", validation.legalName) !== null}
        />
        {#if fieldError("legalName", validation.legalName) !== null}
          <span class="field__error">{fieldError("legalName", validation.legalName)}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">
          Tax number (ADÓSZÁM)
          <span class="field__hint">format: <code>xxxxxxxx-y-zz</code></span>
        </span>
        <input
          class="field__input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          placeholder="24904362-2-41"
          bind:value={form.taxNumber}
          aria-invalid={fieldError("taxNumber", validation.taxNumber) !== null}
        />
        {#if fieldError("taxNumber", validation.taxNumber) !== null}
          <span class="field__error">{fieldError("taxNumber", validation.taxNumber)}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">
          EU VAT number
          <span class="field__hint">optional — only for EU cross-border invoices</span>
        </span>
        <input
          class="field__input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          placeholder="HU24904362"
          bind:value={form.euVatNumber}
        />
      </label>

      <h3 class="wizard__section">Address</h3>

      <label class="field">
        <span class="field__label">
          Country code
          <span class="field__hint">default: HU (Magyarország)</span>
        </span>
        <input
          class="field__input"
          type="text"
          autocomplete="country"
          bind:value={form.addressCountryCode}
          aria-invalid={fieldError("addressCountryCode", validation.addressCountryCode) !== null}
        />
        {#if fieldError("addressCountryCode", validation.addressCountryCode) !== null}
          <span class="field__error">{fieldError("addressCountryCode", validation.addressCountryCode)}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">Postal code</span>
        <input
          class="field__input"
          type="text"
          autocomplete="postal-code"
          bind:value={form.addressPostalCode}
          aria-invalid={fieldError("addressPostalCode", validation.addressPostalCode) !== null}
        />
        {#if fieldError("addressPostalCode", validation.addressPostalCode) !== null}
          <span class="field__error">{fieldError("addressPostalCode", validation.addressPostalCode)}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">City</span>
        <input
          class="field__input"
          type="text"
          autocomplete="address-level2"
          bind:value={form.addressCity}
          aria-invalid={fieldError("addressCity", validation.addressCity) !== null}
        />
        {#if fieldError("addressCity", validation.addressCity) !== null}
          <span class="field__error">{fieldError("addressCity", validation.addressCity)}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">Street</span>
        <input
          class="field__input"
          type="text"
          autocomplete="street-address"
          bind:value={form.addressStreet}
          aria-invalid={fieldError("addressStreet", validation.addressStreet) !== null}
        />
        {#if fieldError("addressStreet", validation.addressStreet) !== null}
          <span class="field__error">{fieldError("addressStreet", validation.addressStreet)}</span>
        {/if}
      </label>

      <h3 class="wizard__section">
        Bank accounts
        <span class="wizard__section-hint">at least one — add more for additional currencies</span>
      </h3>

      <!-- PR-72 / session-94 — multi-row affordance per the ADR-0040
           §addendum scope. The legacy single-bank fields (account
           number / IBAN / bank name / SWIFT) are gone; the operator
           now adds one row per (currency, account_number) and marks
           one default per currency. Validated client-side via
           `validateWizardBankRows`; the per-row POST is wired in
           `onSubmit`. -->
      {#each bankRows as row (row.rowKey)}
        <div class="wizard__bank-row" data-testid="wizard-bank-row-{row.rowKey}">
          <div class="wizard__bank-row-head">
            <span class="wizard__bank-row-label">Bank #{row.rowKey.replace("row-", "")}</span>
            {#if bankRows.length > 1}
              <button
                type="button"
                class="wizard__bank-row-remove"
                onclick={() => removeBankRow(row.rowKey)}
                aria-label="Remove this bank row"
              >Remove</button>
            {/if}
          </div>
          <label class="field">
            <span class="field__label">Currency</span>
            <select class="field__input" bind:value={row.currency}>
              <option value="HUF">HUF</option>
              <option value="EUR">EUR</option>
            </select>
          </label>
          <label class="field">
            <span class="field__label">Account number</span>
            <input
              class="field__input"
              type="text"
              autocomplete="off"
              spellcheck="false"
              bind:value={row.accountNumber}
              aria-invalid={bankRowFieldError(row.rowKey, "accountNumber") !== null}
            />
            {#if bankRowFieldError(row.rowKey, "accountNumber") !== null}
              <span class="field__error">{bankRowFieldError(row.rowKey, "accountNumber")}</span>
            {/if}
          </label>
          <label class="field">
            <span class="field__label">Bank name</span>
            <input
              class="field__input"
              type="text"
              autocomplete="off"
              bind:value={row.bankName}
              aria-invalid={bankRowFieldError(row.rowKey, "bankName") !== null}
            />
            {#if bankRowFieldError(row.rowKey, "bankName") !== null}
              <span class="field__error">{bankRowFieldError(row.rowKey, "bankName")}</span>
            {/if}
          </label>
          <label class="field">
            <span class="field__label">SWIFT / BIC</span>
            <input
              class="field__input"
              type="text"
              autocomplete="off"
              spellcheck="false"
              bind:value={row.swiftBic}
              aria-invalid={bankRowFieldError(row.rowKey, "swiftBic") !== null}
            />
            {#if bankRowFieldError(row.rowKey, "swiftBic") !== null}
              <span class="field__error">{bankRowFieldError(row.rowKey, "swiftBic")}</span>
            {/if}
          </label>
          <label class="field field--checkbox">
            <input type="checkbox" bind:checked={row.setAsDefault} />
            <span>Default for {row.currency}</span>
          </label>
        </div>
      {/each}

      <div class="wizard__bank-add-row">
        <button
          type="button"
          class="wizard__bank-add"
          onclick={addBankRow}
          data-testid="wizard-bank-add"
        >+ Add another bank account</button>
      </div>

      {#if !bankValidation.ok && bankValidation.summary}
        <div class="wizard__error" role="alert" data-testid="wizard-bank-summary-error">
          <strong>Bank rows need attention.</strong>
          <p class="wizard__error-detail">{bankValidation.summary}</p>
        </div>
      {/if}

      {#if bankSubmitError !== null}
        <div class="wizard__error" role="alert">
          <strong>Bank row save failed.</strong>
          <p class="wizard__error-detail">{bankSubmitError}</p>
        </div>
      {/if}

      {#if submitError !== null}
        <div class="wizard__error" role="alert">
          <strong>Could not save seller info.</strong>
          <p class="wizard__error-detail">{submitError}</p>
        </div>
      {/if}

      <div class="wizard__actions">
        <button
          type="submit"
          class="wizard__submit"
          disabled={submitting || !validation.ok || !bankValidation.ok}
        >
          {submitting ? "Saving…" : "Save & continue"}
        </button>
      </div>
    </fieldset>
  </form>
</section>

<style>
  .wizard {
    max-width: 560px;
    margin: var(--space-5) auto;
    padding: var(--space-5);
    background: var(--color-surface-raised);
    border-radius: var(--radius-md);
    border: 1px solid var(--color-surface-divider);
  }

  .wizard__title {
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-lg);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .wizard__lede {
    margin: 0 0 var(--space-4) 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
  }

  .wizard__form {
    display: contents;
  }

  .wizard__fieldset {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    border: 0;
    padding: 0;
    margin: 0;
  }

  .wizard__section {
    margin: var(--space-3) 0 0 0;
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-strong);
    border-bottom: 1px solid var(--color-surface-divider);
    padding-bottom: var(--space-1);
  }

  .wizard__section-hint {
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

  .field__input {
    flex: 1;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .field__input[aria-invalid="true"] {
    border-color: var(--color-signal-negative);
  }

  .field__error {
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
  }

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  .wizard__error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-base, var(--color-surface-raised));
    font-size: var(--type-size-sm);
  }

  .wizard__error-detail {
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .wizard__actions {
    display: flex;
    justify-content: flex-end;
    margin-top: var(--space-2);
  }

  .wizard__submit {
    padding: var(--space-2) var(--space-5);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .wizard__submit:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* PR-72 / session-94 — wizard multi-row bank affordance. */
  .wizard__bank-row {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-2);
    background: var(--color-surface-base, transparent);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
  }

  .wizard__bank-row-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  .wizard__bank-row-label {
    font-size: var(--type-size-xs);
    font-weight: 600;
    color: var(--color-text-muted);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .wizard__bank-row-remove {
    background: transparent;
    border: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    padding: var(--space-1) var(--space-2);
    font-size: var(--type-size-xs);
    cursor: pointer;
  }

  .wizard__bank-row-remove:hover {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .wizard__bank-add-row {
    display: flex;
    justify-content: flex-start;
  }

  .wizard__bank-add {
    padding: var(--space-1) var(--space-3);
    background: transparent;
    color: var(--color-text-primary);
    border: 1px dashed var(--color-surface-divider);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .wizard__bank-add:hover {
    background: var(--color-surface-divider);
  }

  .field--checkbox {
    flex-direction: row;
    align-items: center;
    gap: var(--space-2);
  }
</style>
