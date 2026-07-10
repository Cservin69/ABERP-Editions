<script lang="ts">
  // S438 — "Mark parts" modal. For a Completed defense/aerospace WO, mints a
  // `dp-<ULID>` part UID per unit + records an optional operator serial (blank
  // → server auto-derives). One form, one save ([[hulye-biztos]]): the operator
  // never types a UID — only the serial is editable. After save we show the
  // minted UIDs + DataMatrix payloads so the operator can mark the metal.

  import { markParts, type PartMark } from "../lib/api";
  import { validateSerial } from "../lib/part-uid";

  interface Props {
    woId: string;
    expectedUnits: number;
    onMarked: (marks: PartMark[]) => void;
    onClose: () => void;
  }

  let { woId, expectedUnits, onMarked, onClose }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  let serials = $state<string[]>([]);
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  // After a successful save, the minted marks (read-only confirmation).
  let result = $state<PartMark[] | null>(null);

  // Per-unit client-side pre-validation (backend re-checks authoritatively).
  let serialErrors = $derived(serials.map((s) => validateSerial(s)));
  let canSave = $derived(serialErrors.every((e) => e === null));

  // Seed one empty serial slot per unit. The parent remounts the modal per
  // open, so this fires once per instance (mirrors AssignHeatLotModal).
  $effect(() => {
    serials = Array.from({ length: expectedUnits }, () => "");
  });

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  async function onSubmit(event: Event) {
    event.preventDefault();
    if (!canSave || submitting) return;
    submitError = null;
    submitting = true;
    try {
      const resp = await markParts(woId, serials);
      result = resp.part_marks;
    } catch (err: unknown) {
      submitError = err instanceof Error ? err.message : String(err);
    } finally {
      submitting = false;
    }
  }

  function onDone() {
    if (result) onMarked(result);
    if (dialogEl?.open) dialogEl.close();
    onClose();
  }

  function onCancel() {
    if (dialogEl?.open) dialogEl.close();
    onClose();
  }

  function onDialogClick(event: MouseEvent) {
    if (event.target === dialogEl) {
      // After a save the marks are committed — closing must still surface them.
      onDone();
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="mark-modal"
  onclose={onClose}
  onclick={onDialogClick}
  aria-label="Mark parts"
>
  <div class="frame">
    <header class="head">
      <h2>Alkatrész-jelölés / Mark parts</h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    {#if result === null}
      <p class="lede">
        Mint a part UID for each of the <strong>{expectedUnits}</strong> unit(s) of
        <strong class="mono">{woId}</strong>. The
        <code>dp-…</code> UID and the DataMatrix payload are generated on save —
        you only enter an optional serial (blank auto-derives
        <code>{woId}-N</code>).
      </p>

      <form class="body" onsubmit={onSubmit}>
        <fieldset disabled={submitting} class="units">
          {#each serials as _serial, i (i)}
            <label class="field">
              <span class="field__label">Unit {i + 1} serial (optional)</span>
              <input
                type="text"
                bind:value={serials[i]}
                data-testid={`serial-input-${i}`}
                placeholder={`${woId}-${i + 1}`}
                autocomplete="off"
              />
              {#if serialErrors[i] !== null}
                <span class="field__error">{serialErrors[i]}</span>
              {/if}
            </label>
          {/each}
        </fieldset>

        {#if submitError !== null}
          <div class="error" role="alert">
            <strong>Could not mark parts.</strong>
            <p class="error__detail">{submitError}</p>
          </div>
        {/if}

        <div class="actions">
          <button type="button" class="quiet-button" onclick={onCancel}>Cancel</button>
          <button type="submit" class="primary" disabled={submitting || !canSave}>
            {submitting ? "Marking…" : "Mark / Jelölés"}
          </button>
        </div>
      </form>
    {:else}
      <p class="lede">
        Marked <strong>{result.length}</strong> unit(s). Apply each DataMatrix to
        the matching part:
      </p>
      <ul class="marked" data-testid="marked-list">
        {#each result as m (m.part_uid)}
          <li class="marked__row">
            <span class="marked__idx">#{m.unit_index}</span>
            <span class="marked__uid mono">{m.part_uid}</span>
            <span class="marked__serial">{m.serial_number}</span>
            <code class="marked__dm">{m.data_matrix_payload}</code>
          </li>
        {/each}
      </ul>
      <div class="actions">
        <button type="button" class="primary" onclick={onDone}>Done / Kész</button>
      </div>
    {/if}
  </div>
</dialog>

<style>
  dialog.mark-modal {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    width: 640px;
    overflow: hidden;
  }

  dialog.mark-modal::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) var(--space-5);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .head {
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

  .lede {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
    line-height: 1.5;
  }

  .mono {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .units {
    border: 0;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    max-height: 50vh;
    overflow-y: auto;
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

  .field input {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .field__error {
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
  }

  .marked {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    max-height: 50vh;
    overflow-y: auto;
  }

  .marked__row {
    display: grid;
    grid-template-columns: auto 1fr auto;
    grid-template-areas: "idx uid serial" "dm dm dm";
    gap: var(--space-1) var(--space-2);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-raised);
    font-size: var(--type-size-sm);
  }

  .marked__idx {
    grid-area: idx;
    color: var(--color-text-muted);
  }
  .marked__uid {
    grid-area: uid;
  }
  .marked__serial {
    grid-area: serial;
    color: var(--color-text-secondary);
  }
  .marked__dm {
    grid-area: dm;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    word-break: break-all;
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
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .primary:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    font-size: var(--type-size-sm);
  }

  .error__detail {
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }
</style>
