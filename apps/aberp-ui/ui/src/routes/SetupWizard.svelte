<script lang="ts">
  // PR-46α / session-62 — first-run NAV-credentials wizard. Renders
  // when the Tauri shell reports BootStatus === "needs-setup" (i.e.
  // the keychain is empty for this tenant). Four form fields → POST
  // /api/setup-nav-credentials via the matching Tauri command →
  // backend writes the keychain entries + flips its boot state to
  // Ready → SPA re-mounts the normal app on the next getBootStatus
  // poll. Zero terminal interaction needed.
  //
  // The composer + validator live in `setup-credentials.ts` so vitest
  // can pin the wire shape without mounting this component
  // (component-test runner is named-deferred per CLAUDE.md rule 2;
  // the composer-pin pattern is A156 / A161 / A163 precedent).

  import { setupNavCredentials } from "../lib/api";
  import {
    composeSetupCredentialsBody,
    validateSetupCredentials,
    type SetupCredentialsForm,
  } from "../lib/setup-credentials";

  let form: SetupCredentialsForm = $state({
    technicalUserLogin: "",
    technicalUserPassword: "",
    xmlSignKey: "",
    xmlChangeKey: "",
  });

  // Per-field secret reveal toggles. Default to masked; the operator
  // can flip individually so a long pasted key can be visually
  // verified before submit.
  let showPassword = $state(false);
  let showSignKey = $state(false);
  let showChangeKey = $state(false);

  let submitting = $state(false);
  let submitError: string | null = $state(null);

  let validation = $derived(validateSetupCredentials(form));

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    if (!validation.ok) {
      return;
    }
    submitting = true;
    try {
      const body = composeSetupCredentialsBody(form);
      await setupNavCredentials(body);
      // On success, the Tauri shell flips boot state to "ready".
      // App.svelte's poll picks it up within ~300ms and re-renders
      // against the normal app. No explicit navigation here.
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      submitError = message;
    } finally {
      submitting = false;
    }
  }
</script>

<section class="wizard" role="form" aria-labelledby="wizard-title">
  <h2 id="wizard-title" class="wizard__title">Welcome to ABERP</h2>
  <p class="wizard__lede">
    ABERP needs your NAV technical-user credentials to talk to the tax
    authority's online invoice system. These four values will be stored
    securely in your macOS keychain. You can change them later from the
    settings screen.
  </p>

  <form onsubmit={onSubmit} class="wizard__form">
    <fieldset disabled={submitting} class="wizard__fieldset">
      <label class="field">
        <span class="field__label">Technical-user login</span>
        <input
          class="field__input"
          type="text"
          autocomplete="off"
          autocapitalize="off"
          spellcheck="false"
          bind:value={form.technicalUserLogin}
          aria-invalid={validation.technicalUserLogin !== null}
        />
        {#if validation.technicalUserLogin !== null}
          <span class="field__error">{validation.technicalUserLogin}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">Password</span>
        <div class="field__row">
          <input
            class="field__input"
            type={showPassword ? "text" : "password"}
            autocomplete="new-password"
            bind:value={form.technicalUserPassword}
            aria-invalid={validation.technicalUserPassword !== null}
          />
          <button
            type="button"
            class="field__toggle"
            onclick={() => (showPassword = !showPassword)}
            aria-pressed={showPassword}
          >
            {showPassword ? "Hide" : "Show"}
          </button>
        </div>
        {#if validation.technicalUserPassword !== null}
          <span class="field__error">{validation.technicalUserPassword}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">XML sign key</span>
        <div class="field__row">
          <input
            class="field__input"
            type={showSignKey ? "text" : "password"}
            autocomplete="off"
            spellcheck="false"
            bind:value={form.xmlSignKey}
            aria-invalid={validation.xmlSignKey !== null}
          />
          <button
            type="button"
            class="field__toggle"
            onclick={() => (showSignKey = !showSignKey)}
            aria-pressed={showSignKey}
          >
            {showSignKey ? "Hide" : "Show"}
          </button>
        </div>
        {#if validation.xmlSignKey !== null}
          <span class="field__error">{validation.xmlSignKey}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field__label">XML change (exchange) key</span>
        <div class="field__row">
          <input
            class="field__input"
            type={showChangeKey ? "text" : "password"}
            autocomplete="off"
            spellcheck="false"
            bind:value={form.xmlChangeKey}
            aria-invalid={validation.xmlChangeKey !== null}
          />
          <button
            type="button"
            class="field__toggle"
            onclick={() => (showChangeKey = !showChangeKey)}
            aria-pressed={showChangeKey}
          >
            {showChangeKey ? "Hide" : "Show"}
          </button>
        </div>
        {#if validation.xmlChangeKey !== null}
          <span class="field__error">{validation.xmlChangeKey}</span>
        {/if}
      </label>

      <details class="wizard__help">
        <summary>Where do I get these?</summary>
        <p class="wizard__help-body">
          Sign in to the NAV portal at
          <code>https://onlineszamla.nav.gov.hu</code> with your operator
          account, then navigate to <em>Felhasználók</em> →
          <em>Technikai felhasználó</em>. Each technical user has a login
          and a password; the XML sign key and XML change key appear on
          the same screen.
        </p>
      </details>

      {#if submitError !== null}
        <div class="wizard__error" role="alert">
          <strong>Could not save credentials.</strong>
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
    gap: var(--space-4);
    border: 0;
    padding: 0;
    margin: 0;
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

  .field__row {
    display: flex;
    gap: var(--space-2);
    align-items: stretch;
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

  .field__toggle {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: 4px;
    font-size: var(--type-size-xs);
    cursor: pointer;
  }

  .field__toggle:hover {
    background: var(--color-surface-divider);
  }

  .field__error {
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
  }

  .wizard__help {
    margin-top: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }

  .wizard__help summary {
    cursor: pointer;
    color: var(--color-text-muted);
  }

  .wizard__help-body {
    margin: var(--space-2) 0 0 0;
    line-height: 1.5;
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
