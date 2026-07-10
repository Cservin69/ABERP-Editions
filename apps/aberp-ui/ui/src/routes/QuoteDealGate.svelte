<script lang="ts">
  // S272 / PR-261 — EVE addenda 2 (UI half) + 3.
  //
  // Per-row DEAL affordance for the Quotes list. Renders the typed
  // REFRESH gate when `stockAlert=true`, then a BIG/RED single-use
  // DEAL-token input that switches to a green confirm state when the
  // operator types the row's `expectedDealToken` (the first 8 chars
  // of `quote_id`).
  //
  // # Why a separate component
  //
  // The golden snapshot test (vitest + happy-dom) renders this in
  // isolation across 3 states (empty / wrong / correct) without
  // spinning the whole QuotesList tree.
  //
  // # Server-side defense-in-depth
  //
  // The SPA disables the button until both fields are satisfied, but
  // the saga route validates again. A user-scripted POST that bypasses
  // the SPA gate hits the same `stock_alert_refresh_required` /
  // `deal_token_mismatch` / `deal_already_issued` 409 surfaces per
  // [[trust-code-not-operator]].
  //
  // # Tokens (per [[spa-dark-theme-default]])
  //
  // All colours from `tokens.css`: `--color-signal-negative` for RED
  // affordances (matches stock_alert badge + storno-button surface),
  // `--color-signal-positive` for GREEN confirm, `--color-surface-*`
  // for backgrounds.

  import {
    deriveDealGateState,
    dealSagaErrorTitle,
    REFRESH_ACK_TOKEN,
  } from "../lib/deal-gate-state";

  interface Props {
    /** Quote-id; the expected DEAL token is its first 8 chars. */
    quoteId: string;
    /** Row's current `stock_alert`; gates the REFRESH input visibility. */
    stockAlert: boolean;
    /** Server-side disable: an in-flight POST hides the input + button
     * to prevent double-submit. */
    submitting?: boolean;
    /** Last 409 / submission error to surface below the input. */
    error?: { code: string; message: string } | null;
    /** Called with `{ deal_token, refresh_ack }` once both gates are
     * satisfied. The parent handles the actual saga call so this
     * component stays pure / testable. */
    onSubmit: (payload: { deal_token: string; refresh_ack: string | null }) => void;
  }

  let {
    quoteId,
    stockAlert,
    submitting = false,
    error = null,
    onSubmit,
  }: Props = $props();

  // EVE addenda 2 + 3 — both gate inputs are bound, case-sensitive per
  // [[hulye-biztos]] (no auto-uppercase / lowercase normalisation).
  let refreshInput = $state("");
  let dealInput = $state("");

  // Pure state derivation lives in `../lib/deal-gate-state.ts` so the
  // vitest pin can exercise the same gate logic the component renders.
  let gate = $derived(
    deriveDealGateState({
      quoteId,
      stockAlert,
      refreshInput,
      dealInput,
      submitting,
    }),
  );

  function handleSubmit(): void {
    if (!gate.canSubmit) return;
    onSubmit({
      deal_token: dealInput,
      refresh_ack: stockAlert ? refreshInput : null,
    });
  }
</script>

<section
  class="deal-gate"
  data-testid="deal-gate"
  data-deal-state={gate.dealTone}
  data-refresh-acked={stockAlert ? String(gate.refreshAcked) : "n/a"}
>
  {#if stockAlert}
    <!-- EVE addendum 2 (UI half) — typed REFRESH gate.
         BIG/RED bordered input; hidden once acked. The label is
         intentionally loud — the operator must consciously type the
         literal token, not click an "OK" button. -->
    {#if !gate.refreshAcked}
      <div class="deal-gate__refresh" data-testid="deal-gate-refresh">
        <label
          class="deal-gate__refresh-label"
          for="deal-gate-refresh-input-{quoteId}"
        >
          <span class="deal-gate__refresh-title">
            ⚠ Készletállapot megváltozott — REFRESH kötelező a DEAL előtt
            / Stock status changed — type REFRESH to acknowledge first
          </span>
          <span class="deal-gate__refresh-hint">
            Írja be a <code>REFRESH</code> szót pontosan (kis-/nagybetű
            számít, automatikus átalakítás nincs) /
            Type <code>REFRESH</code> exactly — case-sensitive, no auto-
            uppercase
          </span>
        </label>
        <input
          id="deal-gate-refresh-input-{quoteId}"
          class="deal-gate__refresh-input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          placeholder={REFRESH_ACK_TOKEN}
          bind:value={refreshInput}
          data-testid="deal-gate-refresh-input"
        />
      </div>
    {:else}
      <div
        class="deal-gate__refresh-acked"
        role="status"
        data-testid="deal-gate-refresh-acked"
      >
        ✓ Készletállapot-változás nyugtázva / Stock change acknowledged
      </div>
    {/if}
  {/if}

  {#if !stockAlert || gate.refreshAcked}
    <!-- EVE addendum 3 — BIG / RED / single-use DEAL token field.
         Reveals only AFTER REFRESH ack (or directly when no stock_alert).
         The label includes the expected token verbatim so the operator
         copies the first 8 chars from the row's reference column. -->
    <div class="deal-gate__deal" data-testid="deal-gate-deal-section">
      <label
        class="deal-gate__deal-label"
        for="deal-gate-deal-input-{quoteId}"
      >
        <span class="deal-gate__deal-title">
          DEAL megerősítése / Confirm DEAL
        </span>
        <span class="deal-gate__deal-hint">
          Írja be az ajánlat-azonosító első 8 karakterét pontosan:
          <code data-testid="deal-gate-expected-token">{gate.expectedDealToken}</code>
          / Type the first 8 characters of the quote reference exactly.
        </span>
      </label>
      <input
        id="deal-gate-deal-input-{quoteId}"
        class="deal-gate__deal-input"
        type="text"
        autocomplete="off"
        spellcheck="false"
        placeholder={gate.expectedDealToken}
        bind:value={dealInput}
        data-testid="deal-gate-deal-input"
        data-tone={gate.dealTone}
      />
      {#if gate.dealTone === "wrong"}
        <p
          class="deal-gate__deal-error"
          role="alert"
          data-testid="deal-gate-token-mismatch-hint"
        >
          Nem egyezik a token — másolja az ajánlat-azonosító elejéről
          / Token does not match — copy from the reference column above
        </p>
      {/if}

      <button
        type="button"
        class="deal-gate__deal-button"
        disabled={!gate.canSubmit}
        onclick={handleSubmit}
        data-testid="deal-gate-submit"
        data-tone={gate.dealTone}
      >
        {#if submitting}
          DEAL folyamatban… / Submitting…
        {:else if gate.dealTone === "correct"}
          DEAL kiállítása / Issue DEAL
        {:else}
          DEAL — írja be a tokent / Type the token to confirm
        {/if}
      </button>
    </div>
  {/if}

  {#if error}
    <div
      class="deal-gate__server-error"
      role="alert"
      data-testid="deal-gate-server-error"
      data-error-code={error.code}
    >
      <strong>{dealSagaErrorTitle(error.code)}</strong>
      <p>{error.message}</p>
    </div>
  {/if}
</section>

<style>
  /* Component-level tokens — every colour comes from tokens.css per
     [[spa-dark-theme-default]]. No light-mode fallbacks, no
     undefined-variable shorthands. */

  .deal-gate {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
  }

  /* REFRESH gate (EVE addendum 2 UI half) — RED bordered, big, loud.
     Same `--color-signal-negative` token as the stock_alert banner so
     the operator's eye reads them as one continuous warning surface. */
  .deal-gate__refresh {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    background: var(--color-surface-base);
    border: 2px solid var(--color-signal-negative);
    border-radius: var(--radius-sm);
  }

  .deal-gate__refresh-label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .deal-gate__refresh-title {
    color: var(--color-signal-negative);
    font-weight: 700;
    font-size: var(--type-size-sm);
  }

  .deal-gate__refresh-hint {
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
  }

  .deal-gate__refresh-hint code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
    background: var(--color-surface-raised);
    padding: 0 4px;
    border-radius: var(--radius-sm);
  }

  .deal-gate__refresh-input {
    padding: var(--space-2) var(--space-3);
    border: 2px solid var(--color-signal-negative);
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-lg);
    font-weight: 600;
    border-radius: var(--radius-sm);
    letter-spacing: 0.06em;
  }

  .deal-gate__refresh-acked {
    color: var(--color-signal-positive);
    font-size: var(--type-size-sm);
    font-weight: 600;
  }

  /* DEAL gate (EVE addendum 3) — BIG. RED-bordered when empty/wrong;
     GREEN-bordered when the operator's typed token matches the
     expected first-8-chars. Same affordance weight as the storno
     confirm (S156). */
  .deal-gate__deal {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    background: var(--color-surface-base);
    border-radius: var(--radius-sm);
  }

  .deal-gate__deal-label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .deal-gate__deal-title {
    color: var(--color-text-strong);
    font-weight: 700;
    font-size: var(--type-size-sm);
  }

  .deal-gate__deal-hint {
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
  }

  .deal-gate__deal-hint code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
    background: var(--color-surface-raised);
    padding: 1px 6px;
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
  }

  /* The BIG input: 18px+ font (token-driven), full row width,
     prominent border. Tone is data-driven so the golden snapshot
     can pin all three. NEVER apply autofocus — a tab-key landing on
     a destructive submit is the kind of accident this gate exists to
     prevent. */
  .deal-gate__deal-input {
    padding: var(--space-3);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-lg);
    font-weight: 700;
    letter-spacing: 0.08em;
    border-radius: var(--radius-sm);
    background: var(--color-surface-base);
    color: var(--color-text-strong);
    border: 2px solid var(--color-signal-negative);
  }

  .deal-gate__deal-input[data-tone="correct"] {
    border-color: var(--color-signal-positive);
    color: var(--color-signal-positive);
  }

  .deal-gate__deal-input[data-tone="wrong"] {
    border-color: var(--color-signal-negative);
  }

  .deal-gate__deal-error {
    color: var(--color-signal-negative);
    font-size: var(--type-size-xs);
    margin: 0;
  }

  /* Button is RED background on the wrong/empty tone; GREEN on the
     correct tone. Same loud affordance as the storno-confirm-accept. */
  .deal-gate__deal-button {
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    font-weight: 700;
    cursor: pointer;
    border: 1px solid var(--color-signal-negative);
    background: var(--color-signal-negative);
    color: var(--color-surface-base);
  }

  .deal-gate__deal-button[data-tone="correct"] {
    border-color: var(--color-signal-positive);
    background: var(--color-signal-positive);
    color: var(--color-surface-base);
  }

  .deal-gate__deal-button:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .deal-gate__server-error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    border-radius: var(--radius-sm);
  }

  .deal-gate__server-error strong {
    color: var(--color-signal-negative);
    display: block;
    font-size: var(--type-size-sm);
  }

  .deal-gate__server-error p {
    margin: var(--space-1) 0 0;
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    font-family: var(--type-family-mono);
  }
</style>
