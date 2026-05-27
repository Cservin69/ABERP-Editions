<script lang="ts">
  // PR-53 / session-73 — NAV Credentials settings page. Reads the
  // four-slot presence flags via GET /api/nav-credentials-status and
  // surfaces a per-row "Rotate" affordance for the login + three
  // secrets. The actual rotation goes through
  // POST /api/rotate-nav-credential (single-slot) so the operator can
  // change one value without re-entering the other three.
  //
  // The login row also renders the operator-visible value verbatim;
  // the three secret rows render "✓ present" / "✗ missing" only. This
  // matches the A175 visibility decision: the login is the audit-
  // ledger Actor identifier (operator-readable identity), not a
  // secret; the keychain stores all four for symmetry but only the
  // three secrets are masked at the UI boundary.

  import { onMount } from "svelte";
  import {
    getNavCredentialsStatus,
    rotateNavCredential,
    type NavCredentialsStatusResponse,
    type RotateNavCredentialRequest,
  } from "../lib/api";

  type CredentialSlug = RotateNavCredentialRequest["item"];

  interface RowSpec {
    slug: CredentialSlug;
    label: string;
    secret: boolean;
    hint?: string;
  }

  const ROWS: RowSpec[] = [
    {
      slug: "login",
      label: "Technical-user login",
      secret: false,
      hint: "Audit-ledger actor identity (operator-readable).",
    },
    { slug: "password", label: "Technical-user password", secret: true },
    { slug: "sign_key", label: "XML sign key", secret: true },
    { slug: "change_key", label: "XML change (exchange) key", secret: true },
  ];

  let status: NavCredentialsStatusResponse | null = $state(null);
  let loading = $state(true);
  let loadError: string | null = $state(null);
  /** Slug of the row whose Rotate form is currently open. `null` =
   * no row in rotate mode. Only one row is rotatable at a time so
   * the operator can't half-fill three secrets and lose track. */
  let editingSlug: CredentialSlug | null = $state(null);
  let newValue = $state("");
  let revealValue = $state(false);
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let savedSlug: CredentialSlug | null = $state(null);

  onMount(() => {
    void refresh();
  });

  async function refresh() {
    loading = true;
    loadError = null;
    try {
      status = await getNavCredentialsStatus();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      loadError = message;
    } finally {
      loading = false;
    }
  }

  function openRotate(slug: CredentialSlug) {
    editingSlug = slug;
    newValue = "";
    revealValue = false;
    submitError = null;
    savedSlug = null;
  }

  function cancelRotate() {
    editingSlug = null;
    newValue = "";
    revealValue = false;
    submitError = null;
  }

  async function onSubmit(event: Event) {
    event.preventDefault();
    if (editingSlug === null) return;
    if (newValue.length === 0) {
      submitError = "Value cannot be empty";
      return;
    }
    submitting = true;
    submitError = null;
    try {
      await rotateNavCredential({
        item: editingSlug,
        new_value: newValue,
      });
      savedSlug = editingSlug;
      editingSlug = null;
      newValue = "";
      revealValue = false;
      // Refresh so the row updates "✗ missing" → "✓ present" without
      // a separate poll cadence.
      await refresh();
    } catch (err: unknown) {
      submitError = err instanceof Error ? err.message : String(err);
    } finally {
      submitting = false;
    }
  }

  function presenceLabel(present: boolean): string {
    return present ? "✓ present" : "✗ missing";
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <h2 id="page-title" class="page__title">NAV credentials</h2>
    <p class="page__lede">
      The four technical-user artifacts stored in your macOS keychain
      under the service <code>aberp.nav.&lt;tenant&gt;</code>. Rotate
      individual secrets here when NAV issues a new key; the login is
      operator-visible (audit-ledger actor identity); the three
      secrets are write-only — only their presence is surfaced.
    </p>
  </header>

  {#if loading}
    <p class="page__muted">Loading status…</p>
  {:else if loadError !== null}
    <div class="page__error" role="alert">
      <strong>Could not load NAV credentials status.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else if status !== null}
    <ul class="rows">
      {#each ROWS as row (row.slug)}
        {@const present = row.slug === "login"
          ? status.login
          : row.slug === "password"
            ? status.password
            : row.slug === "sign_key"
              ? status.sign_key
              : status.change_key}
        <li class="row" data-state={present ? "present" : "missing"}>
          <div class="row__main">
            <div class="row__label">{row.label}</div>
            {#if row.slug === "login"}
              <div class="row__value">
                {status.login_value ?? "(not set)"}
              </div>
            {:else}
              <div class="row__value row__value--muted">
                {presenceLabel(present)}
              </div>
            {/if}
            {#if row.hint}
              <div class="row__hint">{row.hint}</div>
            {/if}
            {#if savedSlug === row.slug}
              <div class="row__saved" role="status">Rotated.</div>
            {/if}
          </div>
          <div class="row__actions">
            {#if editingSlug !== row.slug}
              <button
                type="button"
                class="row__btn"
                onclick={() => openRotate(row.slug)}
              >
                {present ? "Rotate" : "Set"}
              </button>
            {/if}
          </div>

          {#if editingSlug === row.slug}
            <form class="rotate" onsubmit={onSubmit}>
              <fieldset disabled={submitting} class="rotate__fieldset">
                <label class="rotate__field">
                  <span class="rotate__label">New value</span>
                  <div class="rotate__row">
                    <input
                      class="rotate__input"
                      type={row.secret && !revealValue ? "password" : "text"}
                      autocomplete="off"
                      spellcheck="false"
                      bind:value={newValue}
                    />
                    {#if row.secret}
                      <button
                        type="button"
                        class="rotate__toggle"
                        onclick={() => (revealValue = !revealValue)}
                        aria-pressed={revealValue}
                      >
                        {revealValue ? "Hide" : "Show"}
                      </button>
                    {/if}
                  </div>
                </label>
                {#if submitError !== null}
                  <p class="rotate__error" role="alert">{submitError}</p>
                {/if}
                <div class="rotate__actions">
                  <button
                    type="button"
                    class="rotate__cancel"
                    onclick={cancelRotate}
                  >
                    Cancel
                  </button>
                  <button
                    type="submit"
                    class="rotate__save"
                    disabled={submitting || newValue.length === 0}
                  >
                    {submitting ? "Saving…" : "Save"}
                  </button>
                </div>
              </fieldset>
            </form>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .page {
    max-width: 720px;
    margin: 0 auto;
  }

  .page__head {
    margin-bottom: var(--space-4);
  }

  .page__title {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-lg);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .page__lede {
    margin: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
  }

  .page__muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  .page__error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    font-size: var(--type-size-sm);
  }

  .page__error-detail {
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .rows {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .row {
    display: grid;
    grid-template-columns: 1fr auto;
    grid-template-areas:
      "main actions"
      "rotate rotate";
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 6px;
  }

  .row[data-state="missing"] {
    border-left: 3px solid var(--color-signal-negative);
  }

  .row[data-state="present"] {
    border-left: 3px solid var(--color-signal-positive);
  }

  .row__main {
    grid-area: main;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .row__actions {
    grid-area: actions;
    align-self: center;
  }

  .row__label {
    font-size: var(--type-size-sm);
    color: var(--color-text-strong);
    font-weight: 500;
  }

  .row__value {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
    word-break: break-all;
  }

  .row__value--muted {
    color: var(--color-text-muted);
  }

  .row__hint {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .row__saved {
    color: var(--color-signal-positive);
    font-size: var(--type-size-xs);
  }

  .row__btn {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    border-radius: 4px;
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .row__btn:hover {
    background: var(--color-surface-divider);
  }

  .rotate {
    grid-area: rotate;
    border-top: 1px solid var(--color-surface-divider);
    padding-top: var(--space-3);
    margin-top: var(--space-1);
  }

  .rotate__fieldset {
    border: 0;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .rotate__field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .rotate__label {
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  .rotate__row {
    display: flex;
    gap: var(--space-2);
    align-items: stretch;
  }

  .rotate__input {
    flex: 1;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .rotate__toggle {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-secondary);
    border-radius: 4px;
    font-size: var(--type-size-xs);
    cursor: pointer;
  }

  .rotate__toggle:hover {
    background: var(--color-surface-divider);
  }

  .rotate__error {
    margin: 0;
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .rotate__actions {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
  }

  .rotate__cancel {
    padding: var(--space-1) var(--space-3);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .rotate__cancel:hover {
    color: var(--color-text-strong);
  }

  .rotate__save {
    padding: var(--space-1) var(--space-4);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: 4px;
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .rotate__save:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
