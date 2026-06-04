// S239 / PR-233 — pure helpers for the InvoiceList Draft-delete flow.
//
// Two surfaces this module shapes:
//
//   1. The confirmation-modal copy (`composeDraftDeleteCopy`). The
//      operator's eye lands on either:
//        - The "this draft is linked to a dispatch" warning when the
//          deleted draft was spawned by a dispatch (the S237 §🔴 #1
//          consequence the cascade then resolves), OR
//        - A simpler "this deletes the draft permanently" warning
//          for standalone drafts (operator-created, no dispatch
//          linkage).
//      Bilingual HU/EN per the project's [[hulye-biztos]] discipline:
//      the operator's first language is Hungarian; the English copy is
//      the audit-evidence reading.
//
//   2. The performDraftDelete wrapper. Same as `deleteInvoiceDraft`
//      from `./api.ts` but typed to return the boolean (per
//      `delete_invoice_draft_request`'s `deleted` outcome) which the
//      caller maps to "row removed, refetch the list" or "row already
//      gone — operator clicked twice — refetch anyway." Wire shape
//      currently returns void; the helper is a thin shim that
//      future-proofs the test by isolating the API surface from the
//      modal's UI logic.

import { deleteInvoiceDraft, getInvoiceDraft } from "./api";

/** Closed-vocab modal copy. The renderer mounts each field verbatim;
 * the consequence sentence is the [[hulye-biztos]] surface (operator
 * sees the consequence BEFORE the destructive action fires). */
export interface DraftDeleteCopy {
  /** Modal title — short label. */
  title: string;
  /** Primary body sentence — explains what is about to happen. */
  body: string;
  /** Optional secondary sentence — only present when a source
   * dispatch link exists; carries the explicit "spawn link will
   * disappear" warning. */
  consequence: string | null;
  /** Label for the destructive confirm button. */
  confirmLabel: string;
  /** Label for the cancel button. */
  cancelLabel: string;
}

/** Compose the modal copy from the draft's source-dispatch linkage.
 *
 * Pure function so the rendering Svelte component stays declarative;
 * the test exercises the branching here without mounting the DOM.
 *
 * @param sourceDispatchId - the dispatch's `dsp_<ULID>` id, or `null`
 *   if the draft is standalone. The raw ULID is fine for now — the
 *   operator can correlate it with the dispatch detail page's URL.
 *   A future `dsp_number` field (Stage 3 Phase ε) would replace this
 *   with the human-readable DSP-YYYY-NNNNN; until then the ULID is
 *   the durable identifier. */
export function composeDraftDeleteCopy(
  sourceDispatchId: string | null,
): DraftDeleteCopy {
  if (sourceDispatchId !== null) {
    return {
      title: "Piszkozat törlése / Delete draft",
      body: `Ez a piszkozat a ${sourceDispatchId} kiszállításhoz kapcsolódik. / This draft is linked to dispatch ${sourceDispatchId}.`,
      consequence:
        "Törlés után a kiszállítás “spawn link”-je megszűnik, és a piszkozat véglegesen eltűnik. / After deletion the dispatch's spawn link is severed and the draft is permanently gone.",
      confirmLabel: "Törlés / Delete",
      cancelLabel: "Mégse / Cancel",
    };
  }
  return {
    title: "Piszkozat törlése / Delete draft",
    body: "A piszkozat véglegesen törlődik. / The draft will be permanently deleted.",
    consequence: null,
    confirmLabel: "Törlés / Delete",
    cancelLabel: "Mégse / Cancel",
  };
}

/** Fetch the draft so the caller can read its `source_dispatch_id`
 * to compose the confirmation copy. Re-exports `getInvoiceDraft`
 * verbatim so the modal-wiring test can mock a single function. */
export async function loadDraftForDeleteConfirm(drfId: string) {
  return getInvoiceDraft(drfId);
}

/** Fire the backend DELETE. Thin shim over `deleteInvoiceDraft` so
 * the modal-wiring test can mock a single function without re-exporting
 * Tauri's `invoke`. */
export async function performDraftDelete(drfId: string): Promise<void> {
  return deleteInvoiceDraft(drfId);
}
