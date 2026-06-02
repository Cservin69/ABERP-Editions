<script lang="ts">
  // PR-217 / S220 — partner-picker modal for restored ExtNav rows.
  //
  // Per [[aberp-extnav-partner-nav-gap]] NAV's `queryInvoiceData
  // OUTBOUND` does NOT expose buyer info for invoices submitted via
  // a third-party invoicing tool (Billingo / KBoss / etc.). The boot-
  // time backfill (S218) is structurally unable to populate
  // `customer_name` for those rows; an operator-paced affordance is
  // the only way to surface the buyer label.
  //
  // Operator workflow:
  //   1. Click the em-dash (or already-linked partner label) in the
  //      Outgoing list's PARTNER column for an ExtNav row.
  //   2. This modal opens; PartnerTypeahead drives the search.
  //   3. Operator picks a partner → POST /api/restored-invoices/:id/partner
  //      → backend writes 4 denorm fields + audit event → modal
  //      closes → parent refreshes the row.
  //   4. Alternatively: operator clicks "Clear" to drop the link
  //      (the row reverts to em-dash); or "Cancel" to abort.

  import PartnerTypeahead from "./PartnerTypeahead.svelte";
  import { setRestoredPartner, type Partner } from "./api";

  interface Props {
    /** `rinv_<ULID>` id of the restored row to annotate. `null` means
     * the modal is closed (parent toggles this to drive open/close). */
    restoredId: string | null;
    /** Current `buyer_name` on the row, for the "currently linked"
     * disclosure at the top of the modal. */
    currentBuyerName: string | null;
    /** Source NAV invoice number, surfaced in the header so the
     * operator knows WHICH row they're annotating. */
    sourceNavInvoiceNumber: string | null;
    /** Fires after a successful POST. Parent should refresh the row
     * (and typically the whole list, since sort/filter on
     * `buyer_name` may shift the row's position). */
    onUpdated?: () => void;
    /** Fires when the operator dismisses the modal (Cancel button,
     * ESC, backdrop click). Parent should set `restoredId = null`. */
    onClose: () => void;
  }

  let {
    restoredId,
    currentBuyerName,
    sourceNavInvoiceNumber,
    onUpdated,
    onClose,
  }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  let searchValue: string = $state("");
  let busy: boolean = $state(false);
  let errorMessage: string | null = $state(null);

  // Open / close the native dialog when the parent toggles
  // `restoredId`. Same pattern as InvoiceDetail's open-on-id $effect.
  $effect(() => {
    if (!dialogEl) return;
    if (restoredId !== null) {
      if (!dialogEl.open) dialogEl.showModal();
      // Reset transient state every time the modal opens.
      searchValue = "";
      busy = false;
      errorMessage = null;
    } else if (dialogEl.open) {
      dialogEl.close();
    }
  });

  function handleDialogClose() {
    onClose();
  }

  // Native dialog backdrop click — same posture as InvoiceDetail.
  function handleDialogClick(e: MouseEvent) {
    if (e.target === dialogEl) {
      dialogEl?.close();
    }
  }

  async function pickPartner(partner: Partner) {
    if (restoredId === null || busy) return;
    busy = true;
    errorMessage = null;
    try {
      await setRestoredPartner(restoredId, partner.id);
      onUpdated?.();
      dialogEl?.close();
    } catch (err: unknown) {
      errorMessage = err instanceof Error ? err.message : String(err);
      busy = false;
    }
  }

  async function clearLink() {
    if (restoredId === null || busy) return;
    busy = true;
    errorMessage = null;
    try {
      await setRestoredPartner(restoredId, null);
      onUpdated?.();
      dialogEl?.close();
    } catch (err: unknown) {
      errorMessage = err instanceof Error ? err.message : String(err);
      busy = false;
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="extnav-partner-picker"
  onclose={handleDialogClose}
  onclick={handleDialogClick}
  aria-label="Link a partner to this externally-submitted invoice"
>
  <div class="picker-frame">
    <header class="picker-header">
      <h3 class="picker-title">
        Partner hozzárendelése / Link partner
      </h3>
      <p class="picker-subtitle">
        NAV nem adja meg a vevő adatait külső szoftverrel (Billingo,
        KBoss, stb.) kiállított számlákhoz. Itt manuálisan
        hozzárendelhetsz egy partnert a saját nyilvántartásodból.
      </p>
      <p class="picker-subtitle picker-subtitle-en">
        NAV does not expose buyer info for invoices submitted via
        other software. Pick a partner from your records here.
      </p>
      {#if sourceNavInvoiceNumber !== null}
        <p class="picker-source mono" title="Raw NAV invoice number">
          {sourceNavInvoiceNumber}
        </p>
      {/if}
      {#if currentBuyerName !== null && currentBuyerName.trim().length > 0}
        <p class="picker-current">
          <span class="picker-current-label"
            >Jelenleg / Currently:</span
          >
          <span class="picker-current-value">{currentBuyerName}</span>
        </p>
      {/if}
    </header>

    <div class="picker-body">
      <label class="picker-search-label" for="extnav-partner-search">
        Partner keresése / Search partner
      </label>
      <PartnerTypeahead
        bind:value={searchValue}
        onSelect={pickPartner}
        inputId="extnav-partner-search"
        ariaLabel="Search partners by name"
        placeholder="Írj be 3+ karaktert / Type 3+ characters…"
      />
      {#if errorMessage !== null}
        <p class="picker-error" role="alert">
          {errorMessage}
        </p>
      {/if}
    </div>

    <footer class="picker-footer">
      {#if currentBuyerName !== null && currentBuyerName.trim().length > 0}
        <button
          type="button"
          class="picker-clear"
          onclick={clearLink}
          disabled={busy}
          title="Drop the current link — the row reverts to em-dash"
        >
          Link törlése / Clear
        </button>
      {/if}
      <button
        type="button"
        class="picker-cancel"
        onclick={() => dialogEl?.close()}
        disabled={busy}
      >
        Mégse / Cancel
      </button>
    </footer>
  </div>
</dialog>

<style>
  .extnav-partner-picker::backdrop {
    background: rgba(0, 0, 0, 0.4);
  }

  .extnav-partner-picker {
    border: none;
    border-radius: 8px;
    padding: 0;
    max-width: 480px;
    width: 90vw;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.2);
  }

  .picker-frame {
    display: flex;
    flex-direction: column;
    gap: 1rem;
    padding: 1.25rem;
    background: var(--surface, white);
    color: var(--text, #222);
  }

  .picker-header {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    border-bottom: 1px solid var(--rule, #e6e6e6);
    padding-bottom: 0.75rem;
  }

  .picker-title {
    margin: 0;
    font-size: 1.1rem;
    font-weight: 600;
  }

  .picker-subtitle {
    margin: 0;
    font-size: 0.85rem;
    color: var(--text-muted, #666);
    line-height: 1.4;
  }

  .picker-subtitle-en {
    font-style: italic;
    color: var(--text-muted-2, #888);
  }

  .picker-source {
    margin: 0.25rem 0 0 0;
    font-size: 0.8rem;
    color: var(--text-muted, #666);
  }

  .picker-current {
    margin: 0.25rem 0 0 0;
    font-size: 0.9rem;
  }

  .picker-current-label {
    color: var(--text-muted, #666);
    margin-right: 0.25rem;
  }

  .picker-current-value {
    font-weight: 500;
  }

  .picker-body {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .picker-search-label {
    font-size: 0.85rem;
    font-weight: 500;
  }

  .picker-error {
    color: var(--danger, #b00020);
    font-size: 0.85rem;
    margin: 0.5rem 0 0 0;
  }

  .picker-footer {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    border-top: 1px solid var(--rule, #e6e6e6);
    padding-top: 0.75rem;
  }

  .picker-clear,
  .picker-cancel {
    padding: 0.4rem 0.85rem;
    border-radius: 4px;
    border: 1px solid var(--rule, #ccc);
    background: var(--surface, white);
    color: var(--text, #222);
    cursor: pointer;
    font-size: 0.9rem;
  }

  .picker-clear:hover:not(:disabled) {
    background: var(--surface-hover, #fafafa);
  }

  .picker-cancel:hover:not(:disabled) {
    background: var(--surface-hover, #fafafa);
  }

  .picker-clear:disabled,
  .picker-cancel:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
