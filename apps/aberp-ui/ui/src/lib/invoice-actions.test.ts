// PR-44η / session-60 — pin tests for the `buttonsForState`
// per-state action-button visibility helper.
//
// Mirror invariant per A163: the per-state visible-button table is
// the load-bearing operator-facing contract. The backend's
// `serve::submit_invoice_request` / `serve::poll_ack_request`
// helpers loud-fail with 409 on state-mismatched POSTs; this table
// keeps the SPA from surfacing a button that would always 409.
// CLAUDE.md rule 9 — per-state coverage means a regression that
// collapses every state to one button list (or returns a constant)
// cannot pass every assertion vacuously.
//
// The eleven `InvoiceState` values are pinned exhaustively below.
// A new state added to the union without a `buttonsForState` arm
// would surface as a TypeScript exhaustiveness error at
// `npm run check` (the function uses a `switch` over the typed
// union with no default arm); this table catches the runtime
// affordance choice.

import { describe, expect, it } from "vitest";

import { buttonsForState, type DetailActionButton } from "./invoice-actions";
import type { InvoiceState } from "./api";

interface Expected {
  state: InvoiceState;
  buttons: DetailActionButton[];
}

const TABLE: Expected[] = [
  // Ready — pre-submission, before any wire attempt. The operator
  // can submit (lights up the only-Ready row) or download the printed
  // PDF (PR-44ε.UI).
  { state: "Ready", buttons: ["Submit", "Download"] },
  // Submitted — Response audit entry exists, no terminal ack yet.
  // The operator can poll for the ack or download.
  { state: "Submitted", buttons: ["PollAck", "Download"] },
  // PendingNavExists — state-2 Pending + Layer-2 Exists evidence.
  // NAV already has the invoice (Layer-2 queryInvoiceCheck answered
  // exists); the operator polls for the ack. Same affordance shape
  // as Submitted per the brief.
  { state: "PendingNavExists", buttons: ["PollAck", "Download"] },
  // Pending — state-2 Pending without Layer-2 evidence. The
  // operator's next move is NAV-recovery (`retry-submission` /
  // `recover-from-nav`) which the SPA does not yet surface. Download
  // only.
  { state: "Pending", buttons: ["Download"] },
  // Recovered — state reconstructed via `recover-from-nav`. The
  // operator's next move is poll-ack against the recovered
  // transactionId — but the chip itself sits above the Submitted
  // line, and PR-44η scope is the standard lifecycle. Download only;
  // a future PR can add a "Poll ack" button on Recovered too.
  { state: "Recovered", buttons: ["Download"] },
  // Finalized — terminal SAVED. PR-47α / session-64: operator can
  // issue a storno (ADR-0023 §1). PR-47β / session-65: operator can
  // also issue a modification (ADR-0024 §6 base case). Download
  // remains available.
  //
  // PR-70 / ADR-0039 — the default (paid=false) baseline includes
  // the "Pay" button for the mark-as-paid affordance. The paid=true
  // branch is pinned by `finalized_paid_hides_pay_button` below.
  {
    state: "Finalized",
    buttons: ["Pay", "Storno", "Modification", "Download"],
  },
  // Rejected — terminal ABORTED. Download only.
  { state: "Rejected", buttons: ["Download"] },
  // Storno — base invoice has a storno chain entry. Download only.
  { state: "Storno", buttons: ["Download"] },
  // Amended — base invoice has a modification chain entry.
  // PR-47β / session-65: modify-after-modify is permitted per
  // ADR-0024 §6 default-permit posture; storno is NOT (a stornoed
  // base + modification is malformed per §6, AND Amended carries no
  // SAVED ack at the top of the chain so ADR-0023 §1's classifier
  // would reject anyway).
  {
    state: "Amended",
    buttons: ["Modification", "Download"],
  },
  // Abandoned — operator marked terminal. Download only.
  { state: "Abandoned", buttons: ["Download"] },
  // Unknown — no entries; nothing actionable but download (which
  // itself will 404 — the SPA still shows the button so the failure
  // is visible per CLAUDE.md rule 12).
  { state: "Unknown", buttons: ["Download"] },
];

describe("buttonsForState", () => {
  for (const { state, buttons } of TABLE) {
    it(`returns [${buttons.join(", ")}] for state=${state}`, () => {
      expect(buttonsForState(state)).toEqual(buttons);
    });
  }

  it("Submit button only appears on Ready", () => {
    // Counter-pin: the only state in the table that includes "Submit"
    // is `Ready`. A regression that surfaced "Submit" on a
    // post-submission state would surface as a 409 from the backend.
    const statesWithSubmit = TABLE.filter((row) =>
      row.buttons.includes("Submit"),
    ).map((row) => row.state);
    expect(statesWithSubmit).toEqual(["Ready"]);
  });

  it("PollAck button only appears on Submitted-class states", () => {
    // Counter-pin: PollAck is visible exactly on the two states the
    // backend's `poll_ack_request` accepts (`Submitted` and
    // `PendingNavExists`). A drift here would diverge the UI from
    // the precondition guard.
    const statesWithPoll = TABLE.filter((row) =>
      row.buttons.includes("PollAck"),
    ).map((row) => row.state);
    expect(statesWithPoll.sort()).toEqual(
      ["PendingNavExists", "Submitted"].sort(),
    );
  });

  it("Storno button only appears on Finalized", () => {
    // PR-47α / session-64 — counter-pin: Storno is visible exactly on
    // the one state the backend's `storno_invoice_request` accepts
    // (`Finalized`). The ADR-0023 §1 precondition + the loud-fail
    // classifier in `check_base_is_finalized` rejects every other
    // state with a named reason; surfacing the button elsewhere would
    // produce a 409 the operator was not warned about.
    const statesWithStorno = TABLE.filter((row) =>
      row.buttons.includes("Storno"),
    ).map((row) => row.state);
    expect(statesWithStorno).toEqual(["Finalized"]);
  });

  it("Modification button only appears on Finalized and Amended", () => {
    // PR-47β / session-65 — counter-pin: Modification is visible on
    // exactly the two states the backend's
    // `modification_invoice_request` accepts (`Finalized` for the
    // base case, `Amended` for modify-after-modify per ADR-0024 §6).
    // The precondition guard at the route + the
    // `check_base_is_modifiable` classifier in `issue_modification.rs`
    // reject every other state with a named reason; surfacing the
    // button elsewhere would produce a 409 the operator was not
    // warned about.
    const statesWithModification = TABLE.filter((row) =>
      row.buttons.includes("Modification"),
    ).map((row) => row.state);
    expect(statesWithModification.sort()).toEqual(
      ["Amended", "Finalized"].sort(),
    );
  });

  it("Download button is present on every state", () => {
    // The printed PDF exists from the moment the draft is created
    // (A155). The download button stays available across the entire
    // lifecycle; a regression that hid it on a non-Ready state would
    // strand the operator without the operator-deliverable artifact.
    for (const { state, buttons } of TABLE) {
      expect(
        buttons.includes("Download"),
        `state=${state} must include Download`,
      ).toBe(true);
    }
  });

  it("covers every InvoiceState union member", () => {
    // Defence-in-depth: a new InvoiceState added without a row here
    // would be silently bucketed into the `default` arm of the
    // switch (there is none — TypeScript catches the missing arm at
    // npm run check), but the runtime helper would throw at the
    // exhaustiveness boundary. This pin asserts the test table
    // covers the eleven labels per ADR-0036 §2.
    const stateNames = TABLE.map((row) => row.state).sort();
    const expected: InvoiceState[] = [
      "Abandoned",
      "Amended",
      "Finalized",
      "Pending",
      "PendingNavExists",
      "Ready",
      "Recovered",
      "Rejected",
      "Storno",
      "Submitted",
      "Unknown",
    ];
    expect(stateNames).toEqual(expected);
  });

  // ── PR-70 / ADR-0039 — Pay-button gating ──────────────────────────

  it("Pay button appears on Finalized when paid=false", () => {
    // The unpaid baseline. Mirror-pin against the default-paid=false
    // branch above; this test explicitly asserts the boolean's
    // contract so a regression that ignored the parameter surfaces.
    expect(buttonsForState("Finalized", false)).toEqual([
      "Pay",
      "Storno",
      "Modification",
      "Download",
    ]);
  });

  it("Pay button is hidden on Finalized when paid=true", () => {
    // The paid branch — operator cannot record a payment twice
    // (backend enforces no-double-pay with 409 per ADR-0039 §3).
    // Surfacing the button on a paid invoice would produce a 409 the
    // operator was not warned about — exactly the failure mode A161
    // / A163 named for the existing per-state buttons.
    expect(buttonsForState("Finalized", true)).toEqual([
      "Storno",
      "Modification",
      "Download",
    ]);
  });

  it("Pay button is never shown on non-Finalized states regardless of paid", () => {
    // Counter-pin: the `paid` flag is a no-op on every other state.
    // The backend's mark-paid route 409s on non-Finalized states; a
    // regression that surfaced Pay on Ready/Submitted/etc. would
    // produce a 409 the operator was not warned about.
    const nonFinalized: InvoiceState[] = [
      "Unknown",
      "Ready",
      "Pending",
      "PendingNavExists",
      "Submitted",
      "Recovered",
      "Rejected",
      "Storno",
      "Amended",
      "Abandoned",
    ];
    for (const state of nonFinalized) {
      expect(buttonsForState(state, false).includes("Pay")).toBe(false);
      expect(buttonsForState(state, true).includes("Pay")).toBe(false);
    }
  });
});
