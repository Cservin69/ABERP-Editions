<script lang="ts">
  // S257 / PR-246 — Add / Edit MES adapter modal. Mirrors the
  // `ConfirmActionModal` / `PartnerForm` `<dialog>` frame; token-styled
  // per [[spa-dark-theme-default]] (every colour resolves to a defined
  // tokens.css variable — no `--color-surface` / `--color-primary` /
  // `--color-danger` shorthand).
  //
  // On Add the kind picker is enabled and the adapter_id is server-
  // minted. On Edit the kind is fixed (changing it is a delete-then-add)
  // and the form PRE-POPULATES from the persisted config — NOT the
  // failed last-known state (S257 adversarial note): an Unhealthy
  // adapter still edits from its config host/port.

  import { untrack } from "svelte";
  import {
    createAdapter,
    updateAdapter,
    type AdapterKind,
    type AdapterListItem,
  } from "../lib/api";
  import { ADAPTER_KIND_LABELS, ADAPTER_KIND_ORDER } from "../lib/adapter-format";

  interface Props {
    mode: "add" | "edit";
    /** Present on edit — the row whose config seeds the form. */
    initial?: AdapterListItem | null;
    /** Called after a successful create/update so the parent can
     * refresh + close. */
    onSaved: () => void;
    onCancel: () => void;
  }

  let { mode, initial = null, onCancel, onSaved }: Props = $props();

  // Plain one-time snapshot of the seed config. The modal is mounted
  // fresh per open (the parent keys it on add/edit), so `initial` never
  // changes during this instance's lifetime — capturing it into form
  // state once is correct (and avoids the `state_referenced_locally`
  // reactive-capture warning). Per the S257 adversarial note the form
  // seeds from the persisted CONFIG, never the failed last-known state.
  // `untrack` is Svelte 5's explicit one-shot-snapshot idiom — same
  // runtime behaviour, and it silences the compile-time warning.
  const seed = untrack(() => initial);

  let kind = $state<AdapterKind>(seed?.kind ?? "barcode-scanner");
  let friendlyName = $state(seed?.friendly_name ?? "");
  let host = $state(seed?.host ?? "");
  let port = $state<number | null>(seed?.port ?? null);
  let deviceName = $state(seed?.device_name ?? "");
  let model = $state(seed?.model ?? "");

  let busy = $state(false);
  let error = $state<string | null>(null);

  let dialogEl = $state<HTMLDialogElement | null>(null);

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  // Barcode `host` is a local bind address; the hint tells the operator
  // it must be an IP, not a hostname (the backend loud-fails otherwise).
  const hostHint = $derived(
    kind === "barcode-scanner"
      ? "Listen IP address (e.g. 127.0.0.1 or 0.0.0.0)"
      : "Host or IP of the device",
  );

  function handleCancel(): void {
    if (busy) return;
    if (dialogEl?.open) dialogEl.close();
    onCancel();
  }

  function onDialogClick(event: MouseEvent): void {
    if (event.target === dialogEl) handleCancel();
  }

  async function handleSave(): Promise<void> {
    if (busy) return;
    error = null;
    const portNum = Number(port);
    if (!Number.isInteger(portNum) || portNum < 1 || portNum > 65535) {
      error = "Port must be an integer between 1 and 65535.";
      return;
    }
    busy = true;
    try {
      // Only thread kind-specific fields for the kind that uses them so
      // a stale device_name typed under a different kind never persists.
      const deviceField =
        kind === "cnc-machine" && deviceName.trim().length > 0
          ? deviceName.trim()
          : null;
      const modelField =
        kind === "robot" && model.trim().length > 0 ? model.trim() : null;

      if (mode === "add") {
        await createAdapter({
          kind,
          friendly_name: friendlyName.trim(),
          host: host.trim(),
          port: portNum,
          device_name: deviceField,
          model: modelField,
        });
      } else if (initial) {
        await updateAdapter(initial.adapter_id, {
          friendly_name: friendlyName.trim(),
          host: host.trim(),
          port: portNum,
          device_name: deviceField,
          model: modelField,
        });
      }
      if (dialogEl?.open) dialogEl.close();
      onSaved();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="adapter-form"
  onclick={onDialogClick}
  aria-label={mode === "add" ? "Add adapter" : "Edit adapter"}
  data-testid="adapter-form"
>
  <div class="frame">
    <header class="head">
      <h2>
        {mode === "add"
          ? "Adapter hozzáadása / Add adapter"
          : "Adapter szerkesztése / Edit adapter"}
      </h2>
    </header>

    {#if error !== null}
      <p class="form-error" role="alert" data-testid="adapter-form-error">
        {error}
      </p>
    {/if}

    <div class="fields">
      <label class="field">
        <span class="field__label">Típus / Kind</span>
        <select
          bind:value={kind}
          disabled={mode === "edit" || busy}
          data-testid="adapter-form-kind"
        >
          {#each ADAPTER_KIND_ORDER as k (k)}
            <option value={k}>{ADAPTER_KIND_LABELS[k]}</option>
          {/each}
        </select>
      </label>

      <label class="field">
        <span class="field__label">Megnevezés / Friendly name</span>
        <input
          type="text"
          bind:value={friendlyName}
          disabled={busy}
          placeholder="Dispatch bench printer"
          data-testid="adapter-form-friendly-name"
        />
      </label>

      <div class="field-row">
        <label class="field field--grow">
          <span class="field__label">Cím / Host</span>
          <input
            type="text"
            bind:value={host}
            disabled={busy}
            placeholder={kind === "barcode-scanner" ? "127.0.0.1" : "10.0.0.5"}
            data-testid="adapter-form-host"
          />
          <span class="field__hint">{hostHint}</span>
        </label>

        <label class="field field--port">
          <span class="field__label">Port</span>
          <input
            type="number"
            min="1"
            max="65535"
            bind:value={port}
            disabled={busy}
            data-testid="adapter-form-port"
          />
        </label>
      </div>

      {#if kind === "cnc-machine"}
        <label class="field">
          <span class="field__label">Eszköznév / Device name</span>
          <input
            type="text"
            bind:value={deviceName}
            disabled={busy}
            placeholder="default"
            data-testid="adapter-form-device-name"
          />
          <span class="field__hint">MTConnect device name (default: "default")</span>
        </label>
      {/if}

      {#if kind === "robot"}
        <label class="field">
          <span class="field__label">Modell / Model</span>
          <input
            type="text"
            bind:value={model}
            disabled={busy}
            placeholder="UR10e"
            data-testid="adapter-form-model"
          />
          <span class="field__hint">UR model label (default: "UR")</span>
        </label>
      {/if}
    </div>

    <footer class="actions">
      <button
        type="button"
        class="quiet-button"
        onclick={handleCancel}
        disabled={busy}
      >
        Mégse / Cancel
      </button>
      <button
        type="button"
        class="primary"
        onclick={() => void handleSave()}
        disabled={busy}
        aria-busy={busy}
        data-testid="adapter-form-save"
      >
        {busy ? "Mentés…" : "Mentés / Save"}
      </button>
    </footer>
  </div>
</dialog>

<style>
  dialog.adapter-form {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    width: 520px;
    overflow: hidden;
  }

  dialog.adapter-form::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-4) var(--space-5);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .head h2 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  .form-error {
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-signal-negative);
    background: var(--color-surface-sunken);
    color: var(--color-signal-negative);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
  }

  .fields {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .field-row {
    display: flex;
    gap: var(--space-3);
  }

  .field--grow {
    flex: 1 1 auto;
  }

  .field--port {
    flex: 0 0 7rem;
  }

  .field__label {
    font-size: var(--type-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-text-muted);
  }

  .field__hint {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .field input,
  .field select {
    padding: var(--space-2);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
    border-radius: var(--radius-sm);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }

  .field input:focus,
  .field select:focus {
    outline: none;
    border-color: var(--color-signal-positive);
  }

  .field input:disabled,
  .field select:disabled {
    opacity: 0.6;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2) var(--space-4);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    border-radius: var(--radius-sm);
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .primary {
    padding: var(--space-2) var(--space-5);
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-signal-positive);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .primary:hover:not(:disabled) {
    color: var(--color-signal-positive);
  }

  .primary:disabled,
  .quiet-button:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
