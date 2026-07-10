<script lang="ts">
  // S166 / prod-prep PR #2 — the one-time first-production-launch
  // confirmation. Mounts (over a blocked main route) when the `/health`
  // probe reports `first_prod_launch_required: true`, i.e. on a
  // production build whose `~/.aberp/prod/.first-launch-acknowledged`
  // touchfile is absent. The operator must type `ABERP` (case-sensitive)
  // and click Proceed; that POSTs the acknowledgement, after which the
  // parent re-probes `/health`, the flag flips false, and this component
  // unmounts. The wizard never re-shows unless the touchfile is deleted.
  //
  // The two decisions (should-show + proceed-enabled) live in
  // `first-prod-launch.ts` as pure functions so they are unit-testable
  // without mounting this component (the project's vitest runs in a
  // node env with no DOM / testing-library — same composer-pin posture
  // as SetupWizard's `setup-credentials.ts`).

  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { acknowledgeFirstProdLaunch } from "../lib/api";
  import { firstProdLaunchProceedEnabled } from "../lib/first-prod-launch";

  // Called by the parent after a successful acknowledgement so it can
  // re-probe `/health` (flipping `first_prod_launch_required` to false,
  // which unmounts this component).
  let { onAcknowledged }: { onAcknowledged: () => void } = $props();

  let typed = $state("");
  let submitting = $state(false);
  let submitError: string | null = $state(null);

  let proceedEnabled = $derived(firstProdLaunchProceedEnabled(typed));

  async function onProceed() {
    if (!proceedEnabled || submitting) return;
    submitError = null;
    submitting = true;
    try {
      await acknowledgeFirstProdLaunch();
      onAcknowledged();
    } catch (err: unknown) {
      submitError = err instanceof Error ? err.message : String(err);
    } finally {
      submitting = false;
    }
  }

  async function onCancel() {
    // Cancel = "I'm not proceeding": quit the app so the operator can
    // relaunch the dev build. Best-effort — if the window API is
    // unavailable the modal simply stays up (the gate holds).
    try {
      await getCurrentWindow().close();
    } catch {
      // no-op — staying blocked is the safe outcome.
    }
  }
</script>

<div class="overlay" role="dialog" aria-modal="true" aria-labelledby="fpl-title">
  <section class="fpl">
    <h2 id="fpl-title" class="fpl__title">
      ABERP ÉLES ÜZEMMÓD — ELSŐ INDÍTÁS
      <span class="fpl__title-en">ABERP PRODUCTION MODE — FIRST LAUNCH</span>
    </h2>

    <!-- Hungarian first, English second (closed-vocab bilingual). -->
    <div class="fpl__body">
      <p>
        Ez a bináris a NAV ÉLES végpontjára (api.onlineszamla.nav.gov.hu)
        küld számlákat valódi hitelesítő adatokkal.
      </p>
      <p>Az ettől a ponttól kiállított számlák:</p>
      <ul>
        <li>jogilag kötelező érvényűek lesznek</li>
        <li>valódi áfakötelezettséget keletkeztetnek</li>
        <li>valódi címzetteknek kerülnek kézbesítésre valódi SMTP-n keresztül</li>
        <li>véglegesen megjelennek a NAV éles nyilvántartásában (törlés nem lehetséges)</li>
      </ul>
      <p class="fpl__quit">
        Ha ez teszt, vagy bizonytalan vagy, LÉPJ KI MOST, és használd a fejlesztői buildet:
        <code>./run/run_desktop.sh</code>
      </p>

      <hr class="fpl__rule" />

      <p>
        This binary submits invoices to NAV's PRODUCTION endpoint
        (api.onlineszamla.nav.gov.hu) using real credentials.
      </p>
      <p>Invoices issued from this point will:</p>
      <ul>
        <li>✓ be legally binding</li>
        <li>✓ trigger real VAT obligations</li>
        <li>✓ be delivered to real recipients via real SMTP</li>
        <li>✓ appear permanently in NAV's production records (no deletion possible)</li>
      </ul>
      <p class="fpl__quit">
        If this is a test or you're unsure, QUIT NOW and use the dev build:
        <code>./run/run_desktop.sh</code>
      </p>
    </div>

    <label class="fpl__confirm">
      <span class="fpl__confirm-label">
        Írd be alább az <strong>ABERP</strong> szót (kis-nagybetű érzékeny) a megerősítéshez. /
        Type <strong>ABERP</strong> (case-sensitive) below to confirm.
      </span>
      <input
        class="fpl__input"
        type="text"
        autocomplete="off"
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
        bind:value={typed}
        disabled={submitting}
        aria-label="Confirmation token"
      />
    </label>

    {#if submitError !== null}
      <p class="fpl__error">{submitError}</p>
    {/if}

    <div class="fpl__actions">
      <button
        type="button"
        class="fpl__cancel"
        onclick={onCancel}
        disabled={submitting}
      >
        Mégse / Cancel
      </button>
      <button
        type="button"
        class="fpl__proceed"
        onclick={onProceed}
        disabled={!proceedEnabled || submitting}
      >
        {submitting ? "Mentés… / Saving…" : "Tovább / Proceed"}
      </button>
    </div>
  </section>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 1000;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: var(--space-5);
    background: rgba(0, 0, 0, 0.72);
    overflow-y: auto;
  }

  .fpl {
    max-width: 620px;
    width: 100%;
    padding: var(--space-5);
    background: var(--color-surface-raised);
    border-radius: var(--radius-md);
    border: 2px solid var(--color-signal-negative);
  }

  .fpl__title {
    margin: 0 0 var(--space-4) 0;
    font-size: var(--type-size-lg);
    font-weight: 700;
    color: var(--color-signal-negative);
    line-height: 1.3;
  }

  .fpl__title-en {
    display: block;
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-secondary);
  }

  .fpl__body {
    color: var(--color-text-primary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
  }

  .fpl__body p {
    margin: 0 0 var(--space-2) 0;
  }

  .fpl__body ul {
    margin: 0 0 var(--space-3) 0;
    padding-left: var(--space-4);
  }

  .fpl__quit {
    color: var(--color-text-strong);
    font-weight: 500;
  }

  .fpl__rule {
    border: 0;
    border-top: 1px solid var(--color-surface-divider);
    margin: var(--space-3) 0;
  }

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  .fpl__confirm {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: var(--space-4) 0 var(--space-2) 0;
  }

  .fpl__confirm-label {
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  .fpl__input {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-md);
  }

  .fpl__error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm);
  }

  .fpl__actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-3);
    margin-top: var(--space-4);
  }

  .fpl__cancel {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    cursor: pointer;
  }

  .fpl__proceed {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--color-signal-negative);
    background: var(--color-signal-negative);
    color: var(--color-surface-base, #fff);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 600;
    cursor: pointer;
  }

  .fpl__proceed:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .fpl__cancel:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
