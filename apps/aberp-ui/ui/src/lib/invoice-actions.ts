// PR-44η / session-60 — operator-action affordance table for the
// invoice-detail modal. Pure-module helper consumed by
// `InvoiceDetail.svelte`; pinned by `invoice-actions.test.ts`.
//
// The mirror invariant per A161 + A163: the per-state visible-button
// table is the load-bearing operator-facing contract. A regression
// that surfaced "Submit to NAV" on an already-`Finalized` invoice
// (or hid it on a `Ready` one) would diverge the UI from the backend's
// precondition guard at `serve::submit_invoice_request`, producing a
// 409 the operator was not warned about. The vitest table pins each
// of the eleven `InvoiceState` values so a regression fails fast at
// `npm test` rather than at operator-survey time per CLAUDE.md rule
// 12 (fail loud).
//
// Pure-module split keeps the helper unit-testable without mounting
// a Svelte 5 component (a component-test runner is named-deferred per
// CLAUDE.md rule 2 — the composer-pin pattern works for every per-
// state UI affordance the modal needs).

import type { InvoiceState } from "./api";

/** Closed vocab of operator-visible action buttons that can appear
 * in the invoice-detail modal header. Kept narrow per CLAUDE.md
 * rule 3 (surgical) — five buttons today (PR-47α added `Storno`,
 * PR-47β added `Modification`); a future PR may add `RetrySubmission`
 * / `Recover` / `MarkAbandoned` here when the SPA surfaces those
 * NAV-recovery affordances. */
export type DetailActionButton =
  | "Submit"
  | "PollAck"
  | "Storno"
  | "Modification"
  | "Download";

/** Per-state action-button visibility table. Returned in operator-
 * reading order (left-to-right on the modal header); the renderer
 * mounts each one as a quiet button. Pinned by
 * `buttonsForState` table tests in `invoice-actions.test.ts`. */
export function buttonsForState(state: InvoiceState): DetailActionButton[] {
  switch (state) {
    case "Ready":
      // Pre-submission: operator can submit or download.
      return ["Submit", "Download"];
    case "Submitted":
    case "PendingNavExists":
      // Submitted but no terminal ack yet: operator polls for the ack.
      // Download stays available throughout the lifecycle per A155 +
      // PR-44ε.UI (the printed invoice exists from the moment the
      // draft is created; the NAV ack does not gate the PDF).
      return ["PollAck", "Download"];
    case "Pending":
      // State-2 Pending without Layer-2 evidence: NAV-recovery is the
      // operator's next move (`retry-submission` / `recover-from-nav`).
      // The SPA does not surface those affordances yet — PR-44η scope
      // is the standard lifecycle only. Download stays available.
      return ["Download"];
    case "Finalized":
      // PR-47α / session-64 — Finalized is the state where storno is
      // legal (ADR-0023 §1: NAV terminal SAVED precondition).
      // PR-47β / session-65 — Finalized is ALSO the base case for
      // modification per ADR-0024 §6 (the `Finalized | Amended`
      // accept set). The backend's `modification_invoice_request`
      // precondition guard mirrors this; surfacing the button on a
      // non-modifiable state would produce a 409 the operator was
      // not warned about.
      return ["Storno", "Modification", "Download"];
    case "Amended":
      // PR-47β / session-65 — Amended is the second arm of the
      // modify-after-modify accept set (ADR-0024 §6 default-permit
      // posture for chains of modifications). Storno is NOT in this
      // arm: ADR-0024 §6 rejects modification of a stornoed base, and
      // ADR-0023 §1 requires Finalized (SAVED) — an Amended base has
      // no remaining SAVED ack at the top of its chain. The CLI's
      // `issue-storno` would loud-fail at the precondition walker.
      return ["Modification", "Download"];
    case "Recovered":
    case "Rejected":
    case "Storno":
    case "Abandoned":
    case "Unknown":
      // Terminal / read-only states: download only.
      return ["Download"];
  }
}
