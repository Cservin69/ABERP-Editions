<script lang="ts">
  // S256 / PR-245 — in-app arrival toast (brief §B.7). Presentational
  // only: the parent (App.svelte) owns the poll loop, the coalescing /
  // de-dup logic (lib/quote-arrival-notifications.ts), and the 8s
  // auto-dismiss timer. Clicking the body navigates to the Quotes tab;
  // the × dismisses without navigating.
  //
  // Dark-theme tokens only ([[spa-dark-theme-default]]) — mirrors the
  // App.svelte `.banner` pattern (signal-positive accent on a raised
  // surface).

  interface Props {
    visible: boolean;
    message: string;
    onView: () => void;
    onDismiss: () => void;
  }
  let { visible, message, onView, onDismiss }: Props = $props();
</script>

{#if visible}
  <div class="quote-toast" role="status" aria-live="polite" data-testid="quote-arrival-toast">
    <button
      type="button"
      class="quote-toast__body"
      onclick={onView}
      data-testid="quote-arrival-toast-view"
    >
      <span class="quote-toast__glyph" aria-hidden="true">📨</span>
      <span class="quote-toast__text">{message}</span>
    </button>
    <button
      type="button"
      class="quote-toast__dismiss"
      aria-label="Dismiss"
      onclick={onDismiss}
      data-testid="quote-arrival-toast-dismiss"
    >
      ×
    </button>
  </div>
{/if}

<style>
  .quote-toast {
    position: fixed;
    bottom: var(--space-5);
    right: var(--space-5);
    z-index: 50;
    display: flex;
    align-items: stretch;
    max-width: 24rem;
    border: 1px solid var(--color-signal-positive);
    background: var(--color-surface-raised);
    border-radius: var(--radius-sm);
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.45);
    animation: quote-toast-in var(--motion-fade-in);
  }

  @keyframes quote-toast-in {
    from {
      opacity: 0;
      transform: translateY(6px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }

  .quote-toast__body {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-3);
    background: transparent;
    border: none;
    cursor: pointer;
    text-align: left;
    color: var(--color-text-primary);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }

  .quote-toast__body:hover .quote-toast__text {
    color: var(--color-signal-positive);
  }

  .quote-toast__glyph {
    font-size: var(--type-size-lg);
    line-height: 1;
  }

  .quote-toast__text {
    color: var(--color-text-strong);
  }

  .quote-toast__dismiss {
    flex: 0 0 auto;
    padding: 0 var(--space-3);
    background: transparent;
    border: none;
    border-left: 1px solid var(--color-surface-divider);
    color: var(--color-text-muted);
    font-size: var(--type-size-lg);
    line-height: 1;
    cursor: pointer;
  }

  .quote-toast__dismiss:hover {
    color: var(--color-text-strong);
  }
</style>
