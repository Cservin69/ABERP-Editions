<script lang="ts">
  // S443 / ADR-0092 — "Record inspection" modal for a Work Order. Mirrors
  // MarkPartsModal.svelte's `<dialog>` pattern. The operator picks an
  // enabled inspection plan (filtered to the WO's product when known),
  // keys the measured value, optionally links a marked part + a probe
  // serial + last-calibration timestamp, and submits. The result shows
  // the computed verdict chip and (if one was auto-created) the NCR id.

  import { onMount } from "svelte";
  import {
    listInspectionPlans,
    recordQcInspection,
    type InspectionPlan,
    type PartMark,
    type QcInspection,
  } from "../lib/api";
  import { verdictChipClass, verdictLabel } from "../lib/verdict";

  interface Props {
    woId: string;
    /** The WO's product id, when the detail carries one — used to filter
     * the plan picker to this product's plans (else all enabled plans). */
    productId: string | null;
    /** The WO's marked parts (for the optional part-UID picker). */
    partMarks: PartMark[];
    /** Invoked after a successful record so the parent reloads the list. */
    onRecorded: () => void;
    onClose: () => void;
  }

  let { woId, productId, partMarks, onRecorded, onClose }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);

  // Plan picker.
  let plans: InspectionPlan[] = $state([]);
  let plansLoadError: string | null = $state(null);
  let selectedPlanId = $state("");
  let selectedPlan = $derived(
    plans.find((p) => p.plan_id === selectedPlanId) ?? null,
  );

  // Form fields.
  let actualValue = $state("");
  let partUid = $state("");
  let probeSerial = $state("");
  let lastCalibrationAt = $state("");

  let submitting = $state(false);
  let submitError: string | null = $state(null);
  // After a successful record: the inspection (verdict chip) + any NCR.
  let result = $state<{ inspection: QcInspection; auto_ncr_id: string | null } | null>(
    null,
  );

  let canSave = $derived(
    selectedPlanId.length > 0 &&
      actualValue.trim().length > 0 &&
      !Number.isNaN(parseFloat(actualValue)) &&
      !submitting,
  );

  onMount(() => {
    void loadPlans();
  });

  $effect(() => {
    if (!dialogEl) return;
    if (!dialogEl.open) dialogEl.showModal();
  });

  async function loadPlans() {
    plansLoadError = null;
    try {
      const resp = await listInspectionPlans(
        productId !== null ? { productId } : undefined,
      );
      // Only enabled plans are pickable for a fresh measurement.
      plans = resp.plans.filter((p) => p.enabled);
    } catch (err: unknown) {
      plansLoadError = err instanceof Error ? err.message : String(err);
    }
  }

  async function onSubmit(event: Event) {
    event.preventDefault();
    if (!canSave) return;
    submitError = null;
    submitting = true;
    try {
      const resp = await recordQcInspection({
        plan_id: selectedPlanId,
        actual_value: parseFloat(actualValue),
        wo_id: woId,
        part_uid: partUid.length > 0 ? partUid : null,
        probe_serial: probeSerial.trim().length > 0 ? probeSerial.trim() : null,
        last_calibration_at:
          lastCalibrationAt.length > 0 ? lastCalibrationAt : null,
      });
      result = {
        inspection: resp.inspection,
        auto_ncr_id: resp.inspection.auto_ncr_id,
      };
    } catch (err: unknown) {
      submitError = err instanceof Error ? err.message : String(err);
    } finally {
      submitting = false;
    }
  }

  function onDone() {
    if (result) onRecorded();
    if (dialogEl?.open) dialogEl.close();
    onClose();
  }

  function onCancel() {
    if (dialogEl?.open) dialogEl.close();
    onClose();
  }

  function onDialogClick(event: MouseEvent) {
    if (event.target === dialogEl) {
      // A recorded inspection is committed; closing must still surface it.
      onDone();
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="rec-modal"
  onclose={onClose}
  onclick={onDialogClick}
  aria-label="Record inspection"
>
  <div class="frame">
    <header class="head">
      <h2>Ellenőrzés rögzítése / Record inspection</h2>
      <button type="button" class="quiet-button" onclick={onCancel} aria-label="Cancel">
        Cancel
      </button>
    </header>

    {#if result === null}
      <p class="lede">
        Record a measured value for <strong class="mono">{woId}</strong>. The
        deviation + verdict are computed on save; an out-of-tolerance result
        auto-creates an NCR.
      </p>

      {#if plansLoadError !== null}
        <div class="error" role="alert">
          <strong>Could not load inspection plans.</strong>
          <p class="error__detail">{plansLoadError}</p>
        </div>
      {/if}

      <form class="body" onsubmit={onSubmit}>
        <fieldset disabled={submitting} class="fields">
          <label class="field">
            <span class="field__label">Terv / Plan *</span>
            <select bind:value={selectedPlanId} data-testid="rec-plan">
              <option value="" disabled>— select a plan —</option>
              {#each plans as p (p.plan_id)}
                <option value={p.plan_id}>
                  {p.feature_name} ({p.units})
                </option>
              {/each}
            </select>
            {#if plans.length === 0 && plansLoadError === null}
              <span class="field__hint">No enabled plans for this product.</span>
            {/if}
          </label>

          <label class="field">
            <span class="field__label">
              Mért érték / Actual value *
              {#if selectedPlan !== null}
                <span class="field__hint">{selectedPlan.units}</span>
              {/if}
            </span>
            <input
              type="number"
              step="any"
              bind:value={actualValue}
              autocomplete="off"
              data-testid="rec-actual"
            />
            {#if selectedPlan !== null}
              <span class="field__hint">
                Nominal {selectedPlan.nominal_value}, tol
                [{selectedPlan.lower_tol}, {selectedPlan.upper_tol}]
                {selectedPlan.units}
              </span>
            {/if}
          </label>

          {#if partMarks.length > 0}
            <label class="field">
              <span class="field__label">Alkatrész / Part UID (optional)</span>
              <select bind:value={partUid} data-testid="rec-part-uid">
                <option value="">— none —</option>
                {#each partMarks as m (m.part_uid)}
                  <option value={m.part_uid}>{m.part_uid}</option>
                {/each}
              </select>
            </label>
          {/if}

          <label class="field">
            <span class="field__label">Szonda sorszám / Probe serial (optional)</span>
            <input
              type="text"
              bind:value={probeSerial}
              autocomplete="off"
              data-testid="rec-probe-serial"
            />
          </label>

          <label class="field">
            <span class="field__label">
              Utolsó kalibráció / Last calibration (optional)
            </span>
            <input
              type="datetime-local"
              bind:value={lastCalibrationAt}
              data-testid="rec-last-cal"
            />
          </label>
        </fieldset>

        {#if submitError !== null}
          <div class="error" role="alert">
            <strong>Could not record inspection.</strong>
            <p class="error__detail">{submitError}</p>
          </div>
        {/if}

        <div class="actions">
          <button type="button" class="quiet-button" onclick={onCancel}>Cancel</button>
          <button type="submit" class="primary" disabled={!canSave}>
            {submitting ? "Recording…" : "Record / Rögzít"}
          </button>
        </div>
      </form>
    {:else}
      <div class="result" data-testid="rec-result">
        <p class="lede">
          Recorded inspection of
          <strong>{result.inspection.feature_name}</strong>:
        </p>
        <dl class="result__grid">
          <dt>Verdict</dt>
          <dd>
            <span class={verdictChipClass(result.inspection.verdict)}>
              {verdictLabel(result.inspection.verdict)}
            </span>
          </dd>
          <dt>Actual</dt>
          <dd class="mono">{result.inspection.actual_value} {result.inspection.units}</dd>
          <dt>Deviation</dt>
          <dd class="mono">{result.inspection.deviation}</dd>
        </dl>
        {#if result.auto_ncr_id !== null}
          <p class="result__ncr" data-testid="rec-ncr">
            NCR created: <strong class="mono">{result.auto_ncr_id}</strong>
          </p>
        {/if}
      </div>
      <div class="actions">
        <button type="button" class="primary" onclick={onDone}>Done / Kész</button>
      </div>
    {/if}
  </div>
</dialog>

<style>
  dialog.rec-modal {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    width: 560px;
    overflow: hidden;
  }

  dialog.rec-modal::backdrop {
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

  .fields {
    border: 0;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
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

  .field input,
  .field select {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-sm);
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
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

  .result__grid {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0;
  }

  .result__grid dt {
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }

  .result__grid dd {
    margin: 0;
    font-size: var(--type-size-sm);
  }

  .result__ncr {
    margin: var(--space-2) 0 0 0;
    font-size: var(--type-size-sm);
    color: var(--color-signal-negative);
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

  /* S443 — verdict chip (shared mapping via verdictChipClass). */
  .verdict-chip {
    display: inline-block;
    padding: 0 var(--space-2);
    border-radius: var(--radius-lg);
    border: 1px solid var(--color-surface-divider);
    font-size: var(--type-size-xs);
    font-weight: 500;
  }
  .verdict-chip--pass {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }
  .verdict-chip--warning {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  .verdict-chip--critical {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .verdict-chip--stale {
    color: var(--color-signal-muted);
    border-color: var(--color-signal-muted);
  }
</style>
