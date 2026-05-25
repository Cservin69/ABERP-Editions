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

  import { setupSellerInfo } from "../lib/api";
  import {
    composeSellerConfigBody,
    DEFAULT_SELLER_CONFIG_FORM,
    parseSetupSellerInfoErrorBody,
    validateSellerConfig,
    type SellerConfigForm,
  } from "../lib/seller-config";

  let form: SellerConfigForm = $state({ ...DEFAULT_SELLER_CONFIG_FORM });
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let fieldErrors: Record<string, string> = $state({});

  let validation = $derived(validateSellerConfig(form));

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    fieldErrors = {};
    if (!validation.ok) {
      return;
    }
    submitting = true;
    try {
      const body = composeSellerConfigBody(form);
      await setupSellerInfo(body);
      // On success the Tauri shell has flipped the boot-state mirror
      // to Ready (via mark_post_setup_state). App.svelte's poll
      // picks it up within ~300ms and re-renders against the normal
      // app. No explicit navigation here.
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
      } else {
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
        Bank info
        <span class="wizard__section-hint">optional — appears on the printed-invoice footer</span>
      </h3>

      <label class="field">
        <span class="field__label">Bank account number</span>
        <input
          class="field__input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          bind:value={form.bankAccountNumber}
        />
      </label>

      <label class="field">
        <span class="field__label">IBAN</span>
        <input
          class="field__input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          bind:value={form.iban}
        />
      </label>

      <label class="field">
        <span class="field__label">Bank name</span>
        <input
          class="field__input"
          type="text"
          autocomplete="off"
          bind:value={form.bankName}
        />
      </label>

      <label class="field">
        <span class="field__label">SWIFT / BIC</span>
        <input
          class="field__input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          bind:value={form.swiftBic}
        />
      </label>

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
          disabled={submitting || !validation.ok}
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
    border-radius: 6px;
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
    border-radius: 4px;
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
    border-radius: 4px;
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .wizard__submit:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
