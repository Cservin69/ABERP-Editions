// PR-44η / session-60 — pin tests for the `buttonsForState`
// per-state action-button visibility helper.
//
// Mirror invariant per A163: the per-state visible-button table is
// the load-bearing operator-facing contract. The backend's
// `serve::submit_invoice_request` helper loud-fails with 409 on
// state-mismatched POSTs; this table keeps the SPA from surfacing
// a button that would always 409. CLAUDE.md rule 9 — per-state
// coverage means a regression that collapses every state to one
// button list (or returns a constant) cannot pass every assertion
// vacuously.
//
// PR-95 / session-115 — the `PollAck` button was removed from the
// closed-vocab; the new 4-state NAV-status pictogram
// (`./nav-status-pictogram.ts`) replaces the manual poll affordance.
// The Submitted / PendingNavExists rows below therefore no longer
// list `PollAck`; the pictogram counter-pin lives in
// `./nav-status-pictogram.test.ts`.
//
// The eleven `InvoiceState` values are pinned exhaustively below.
// A new state added to the union without a `buttonsForState` arm
// would surface as a TypeScript exhaustiveness error at
// `npm run check` (the function uses a `switch` over the typed
// union with no default arm); this table catches the runtime
// affordance choice.

import { describe, expect, it } from "vitest";

import {
  actionGroupLabel,
  buttonsForState,
  detailActionMeta,
  emailButtonState,
  groupButtons,
  navSubmitButtonState,
  type ActionGroup,
  type DetailActionButton,
} from "./invoice-actions";
import type { AuditEntryView, InvoiceState } from "./api";

/** Minimal audit-entry factory for the session-162 button-state pins.
 * Only `kind` + `payload` drive the derivations; the other fields are
 * filled with placeholders so the fixtures read as real ledger rows. */
function entry(kind: string, payload: unknown = null): AuditEntryView {
  return {
    seq: 0,
    kind,
    actor: "test",
    occurred_at: "2026-05-30T10:00:00Z",
    chain_base_invoice_id: null,
    payload,
  };
}

interface Expected {
  state: InvoiceState;
  buttons: DetailActionButton[];
}

const TABLE: Expected[] = [
  // Ready — pre-submission, before any wire attempt. The operator
  // can submit (lights up the only-Ready row), email the PDF to the
  // buyer (PR-92), or download (PR-44ε.UI).
  { state: "Ready", buttons: ["Submit", "Email", "Download"] },
  // Submitted — Response audit entry exists, no terminal ack yet.
  // PR-95 / session-115 — the pictogram (clickable when InFlight)
  // carries the poll affordance; the action bar drops the PollAck
  // button.
  // Session 162 — "Submit" re-surfaces here as a DISABLED in-flight
  // indicator ("Beküldés folyamatban… / Submitting…" via
  // `navSubmitButtonState`) so the operator sees the daemon's
  // auto-submit is under way; it never fires a second submit (the
  // backend would 409 a re-submit on a non-`Ready` state).
  { state: "Submitted", buttons: ["Submit", "Email", "Download"] },
  // PendingNavExists — state-2 Pending + Layer-2 Exists evidence.
  // No Submit (re-submit is 409-gated to `Ready`; the pictogram owns
  // re-poll); Email + Download. Session 162 keeps this row distinct
  // from Submitted so the disabled in-flight indicator shows ONLY on
  // the genuine operator-submitted-NAV-processing state.
  { state: "PendingNavExists", buttons: ["Email", "Download"] },
  // Pending — state-2 Pending without Layer-2 evidence. The
  // operator's next move is NAV-recovery (`retry-submission` /
  // `recover-from-nav`) which the SPA does not yet surface. Email +
  // Download.
  { state: "Pending", buttons: ["Email", "Download"] },
  // Recovered — state reconstructed via `recover-from-nav`. The
  // operator's next move is poll-ack against the recovered
  // transactionId — but the chip itself sits above the Submitted
  // line, and PR-44η scope is the standard lifecycle. Email +
  // Download; a future PR can add a "Poll ack" button on Recovered too.
  { state: "Recovered", buttons: ["Email", "Download"] },
  // Finalized — terminal SAVED. PR-47α / session-64: operator can
  // issue a storno (ADR-0023 §1). PR-47β / session-65: operator can
  // also issue a modification (ADR-0024 §6 base case). Email +
  // Download stay available.
  //
  // PR-70 / ADR-0039 — the default (paid=false) baseline includes
  // the "Pay" button for the mark-as-paid affordance. The paid=true
  // branch is pinned by `finalized_paid_hides_pay_button` below.
  //
  // PR-92 / ADR-0047 — Email is always available on Finalized so
  // the operator can resend.
  {
    state: "Finalized",
    buttons: ["Pay", "Storno", "Modification", "Email", "Download"],
  },
  // Rejected — terminal ABORTED. Email + Download.
  { state: "Rejected", buttons: ["Email", "Download"] },
  // Storno — base invoice has a storno chain entry. Email + Download
  // (operator may resend the original to a buyer for their records).
  { state: "Storno", buttons: ["Email", "Download"] },
  // Amended — base invoice has a modification chain entry.
  // PR-47β / session-65: modify-after-modify is permitted per
  // ADR-0024 §6 default-permit posture; storno is NOT (a stornoed
  // base + modification is malformed per §6, AND Amended carries no
  // SAVED ack at the top of the chain so ADR-0023 §1's classifier
  // would reject anyway).
  {
    state: "Amended",
    buttons: ["Modification", "Email", "Download"],
  },
  // Abandoned — operator marked terminal. Email + Download.
  { state: "Abandoned", buttons: ["Email", "Download"] },
  // Unknown — no entries; nothing actionable but email + download
  // (which itself will 404 — the SPA still shows the buttons so the
  // failure is visible per CLAUDE.md rule 12).
  { state: "Unknown", buttons: ["Email", "Download"] },
  // S236 / S239 — pre-allocation Draft. No NAV number, no PDF, no
  // sequence slot burned; the row's only legal operator affordance
  // is Delete (cascades the dispatch's spawn-link pointer in one tx
  // per S237 §🔴 #1). PR-233 surfaces this through the
  // ConfirmActionModal so the consequence is named before the
  // destructive action fires.
  { state: "Draft", buttons: ["Delete"] },
];

describe("buttonsForState", () => {
  for (const { state, buttons } of TABLE) {
    it(`returns [${buttons.join(", ")}] for state=${state}`, () => {
      expect(buttonsForState(state)).toEqual(buttons);
    });
  }

  it("Submit button appears only on Ready (clickable) and Submitted (disabled in-flight)", () => {
    // Counter-pin: the Submit button surfaces on exactly two states.
    // `Ready` — the only state where `submit_invoice_request` accepts a
    // POST (clickable). `Submitted` — session 162's DISABLED in-flight
    // indicator; the component renders it via `navSubmitButtonState`
    // with `disabled: true`, so it never fires a second submit (the
    // backend would 409 a re-submit on a non-`Ready` state). A
    // regression that surfaced a CLICKABLE Submit on any post-Ready
    // state would produce that 409; the disabled-on-Submitted invariant
    // is pinned by the `navSubmitButtonState` tests below.
    const statesWithSubmit = TABLE.filter((row) =>
      row.buttons.includes("Submit"),
    ).map((row) => row.state);
    expect(statesWithSubmit).toEqual(["Ready", "Submitted"]);
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

  it("Download button is present on every allocated state", () => {
    // The printed PDF exists from the moment a `Ready+` row is
    // allocated (A155). The download button stays available across
    // the entire allocated lifecycle; a regression that hid it on a
    // non-Ready allocated state would strand the operator without
    // the operator-deliverable artifact. The pre-allocation Draft
    // state is the exception — no PDF exists yet — and is
    // explicitly skipped here.
    for (const { state, buttons } of TABLE) {
      if (state === "Draft") continue;
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
    // covers every InvoiceState label (eleven post-S236 + Draft = twelve).
    const stateNames = TABLE.map((row) => row.state).sort();
    const expected: InvoiceState[] = [
      "Abandoned",
      "Amended",
      "Draft",
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
      "Email",
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
      "Email",
      "Download",
    ]);
  });

  // ── PR-92 / ADR-0047 — Email button pins ──────────────────────────

  it("Email button is present on every allocated state", () => {
    // PR-92 — the PDF exists from the moment an `inv_*` row is
    // allocated (A155), so the SMTP send button is available at any
    // point in the allocated lifecycle. Even terminal states
    // (Rejected / Storno / Abandoned) keep the button so the
    // operator can resend the original to a buyer who lost the
    // email. The pre-allocation Draft state is the exception (no
    // PDF yet, no email-able artifact) and is explicitly skipped.
    for (const { state, buttons } of TABLE) {
      if (state === "Draft") continue;
      expect(
        buttons.includes("Email"),
        `state=${state} must include Email`,
      ).toBe(true);
    }
  });

  it("Email button lands in the Export group", () => {
    // PR-92 — Email is a buyer-deliverable artifact channel just like
    // Download (PDF in mailbox vs PDF on disk). Both sit in Export so
    // the operator sees them in the same visual section.
    expect(detailActionMeta("Email").group).toBe("Export");
  });

  // ── PR-80 / session-102 — Action metadata + grouping pins ─────────

  it("detailActionMeta returns load-bearing fields for every button", () => {
    // Pin the closed-vocab metadata table: every variant must surface
    // a glyph, both labels (HU + EN), a tooltip, and a group. A
    // regression that omitted any of these would render a half-formed
    // affordance — the operator would see a button without a label.
    const all: DetailActionButton[] = [
      "Submit",
      "Storno",
      "Modification",
      "Pay",
      "Download",
      "Email",
    ];
    for (const button of all) {
      const meta = detailActionMeta(button);
      expect(meta.glyph.length).toBeGreaterThan(0);
      expect(meta.label_hu.length).toBeGreaterThan(0);
      expect(meta.label_en.length).toBeGreaterThan(0);
      expect(meta.tooltip_hu.length).toBeGreaterThan(0);
      expect(["Lifecycle", "Operational", "Chain", "Export"]).toContain(
        meta.group,
      );
    }
  });

  it("detailActionMeta groups each button into its expected section", () => {
    // The group assignment is load-bearing: it drives the visual
    // hierarchy on the action bar. A regression that bucketed Storno
    // into Lifecycle would render the storno button under the NAV-
    // ladder section header, misleading the operator about what the
    // action does.
    expect(detailActionMeta("Submit").group).toBe("Lifecycle");
    expect(detailActionMeta("Pay").group).toBe("Operational");
    expect(detailActionMeta("Storno").group).toBe("Chain");
    expect(detailActionMeta("Modification").group).toBe("Chain");
    expect(detailActionMeta("Download").group).toBe("Export");
    expect(detailActionMeta("Email").group).toBe("Export");
  });

  it("groupButtons preserves canonical group order", () => {
    // The canonical order is Lifecycle → Operational → Chain → Export
    // (matches the operator's eye-flow: NAV-ladder first, operational
    // recording next, chain children, export last). A regression that
    // re-ordered the groups would force the operator to re-learn the
    // layout.
    const result = groupButtons([
      "Download",
      "Storno",
      "Pay",
      "Modification",
      "Submit",
    ]);
    expect(result.map((r) => r.group)).toEqual([
      "Lifecycle",
      "Operational",
      "Chain",
      "Export",
    ]);
  });

  it("groupButtons preserves per-group reading order", () => {
    // Within each group the input order is preserved so the operator
    // sees the same per-state ordering `buttonsForState` returned.
    const result = groupButtons(["Storno", "Modification"]);
    expect(result).toEqual([
      { group: "Chain", buttons: ["Storno", "Modification"] },
    ]);
  });

  it("groupButtons omits empty groups", () => {
    // A Ready state has [Submit, Download] only; the rendered bar
    // should NOT show an empty `Operational` or `Chain` section
    // header (which would render as an orphan label with no buttons).
    const result = groupButtons(["Submit", "Download"]);
    expect(result.map((r) => r.group)).toEqual(["Lifecycle", "Export"]);
  });

  it("groupButtons of buttonsForState('Finalized') yields Operational/Chain/Export", () => {
    // Mirror-pin: Finalized (unpaid) is the most-action-rich state on
    // the regulatory ladder — Pay (Operational), Storno+Modification
    // (Chain), Email+Download (Export). Lifecycle is empty because
    // the NAV ladder has already terminated at SAVED. A regression
    // collapsing a section would surface as a missing label here.
    const buttons = buttonsForState("Finalized", false);
    const groups = groupButtons(buttons).map((g) => g.group);
    expect(groups).toEqual(["Operational", "Chain", "Export"]);
  });

  it("groupButtons of buttonsForState('Ready') yields Lifecycle/Export", () => {
    // Pre-submission state — Submit (Lifecycle) + Download (Export).
    // The Operational and Chain sections are absent so their labels
    // don't render.
    const buttons = buttonsForState("Ready", false);
    const groups = groupButtons(buttons).map((g) => g.group);
    expect(groups).toEqual(["Lifecycle", "Export"]);
  });

  it("groupButtons of buttonsForState('Submitted') yields Lifecycle/Export", () => {
    // Mid-flight state — PR-95 / session-115 dropped the PollAck
    // button (the pictogram carries the poll affordance). Session 162
    // re-surfaces "Submit" (Lifecycle) as a DISABLED in-flight
    // indicator, so the Submitted action bar shows the Lifecycle
    // section (the "Submitting…" button) plus Export (Email +
    // Download). No Operational or Chain section renders.
    const buttons = buttonsForState("Submitted", false);
    const groups = groupButtons(buttons).map((g) => g.group);
    expect(groups).toEqual(["Lifecycle", "Export"]);
  });

  it("actionGroupLabel returns bilingual labels for every group", () => {
    // Closed-vocab guard: every ActionGroup variant has a bilingual
    // label. A new variant would force this test to fail at the
    // TypeScript exhaustiveness check on the switch.
    const groups: ActionGroup[] = [
      "Lifecycle",
      "Operational",
      "Chain",
      "Export",
    ];
    for (const group of groups) {
      const label = actionGroupLabel(group);
      expect(label.label_hu.length).toBeGreaterThan(0);
      expect(label.label_en.length).toBeGreaterThan(0);
    }
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
      "Draft",
    ];
    for (const state of nonFinalized) {
      expect(buttonsForState(state, false).includes("Pay")).toBe(false);
      expect(buttonsForState(state, true).includes("Pay")).toBe(false);
    }
  });
});

// Session 162 — audit-driven button-state helpers. Each kind is pinned
// with a distinct (label, glyph, disabled) triple so a derivation that
// collapsed to a constant cannot pass vacuously (CLAUDE.md rule 9).
describe("navSubmitButtonState", () => {
  it("no submission attempt → not_submitted (enabled, 'Beküldés a NAV-hoz')", () => {
    const s = navSubmitButtonState([entry("InvoiceDraftCreated")]);
    expect(s.kind).toBe("not_submitted");
    expect(s.label_hu).toBe("Beküldés a NAV-hoz");
    expect(s.label_en).toBe("Submit to NAV");
    expect(s.disabled).toBe(false);
  });

  it("attempt + no terminal ack → in_flight (DISABLED, 'Beküldés folyamatban…')", () => {
    // The post-issue daemon has POSTed but NAV is still PROCESSING.
    const s = navSubmitButtonState([
      entry("InvoiceSubmissionAttempt"),
      entry("InvoiceSubmissionResponse"),
      entry("InvoiceAckStatus", { ack_status: "PROCESSING" }),
    ]);
    expect(s.kind).toBe("in_flight");
    expect(s.label_hu).toBe("Beküldés folyamatban…");
    expect(s.label_en).toBe("Submitting…");
    // Disabled so the operator can't double-submit (a re-submit on a
    // non-`Ready` state would 409 at the backend).
    expect(s.disabled).toBe(true);
  });

  it("terminal SAVED ack → saved (disabled — re-submit is 409-gated)", () => {
    const s = navSubmitButtonState([
      entry("InvoiceSubmissionAttempt"),
      entry("InvoiceAckStatus", { ack_status: "SAVED" }),
    ]);
    expect(s.kind).toBe("saved");
    expect(s.disabled).toBe(true);
  });

  it("SAVED wins over a prior PROCESSING (precedence, not order)", () => {
    const s = navSubmitButtonState([
      entry("InvoiceSubmissionAttempt"),
      entry("InvoiceAckStatus", { ack_status: "PROCESSING" }),
      entry("InvoiceAckStatus", { ack_status: "SAVED" }),
    ]);
    expect(s.kind).toBe("saved");
  });

  it("terminal ABORTED ack → failed", () => {
    const s = navSubmitButtonState([
      entry("InvoiceSubmissionAttempt"),
      entry("InvoiceAckStatus", { ack_status: "ABORTED" }),
    ]);
    expect(s.kind).toBe("failed");
    expect(s.label_hu).toBe("Beküldés sikertelen");
    expect(s.disabled).toBe(true);
  });

  it("transport-class InvoiceSubmissionAttemptFailed → failed", () => {
    const s = navSubmitButtonState([
      entry("InvoiceSubmissionAttempt"),
      entry("InvoiceSubmissionAttemptFailed", { error_class: "transport" }),
    ]);
    expect(s.kind).toBe("failed");
  });

  it("empty audit trail → not_submitted", () => {
    expect(navSubmitButtonState([]).kind).toBe("not_submitted");
  });
});

describe("emailButtonState", () => {
  it("no email attempt → idle ('Email a vevőnek', ✉)", () => {
    const s = emailButtonState([entry("InvoiceDraftCreated")]);
    expect(s.kind).toBe("idle");
    expect(s.label_hu).toBe("Email a vevőnek");
    expect(s.label_en).toBe("Email to buyer");
    expect(s.glyph).toBe("✉");
  });

  it("succeeded send → sent ('Újraküldés', ↻)", () => {
    const s = emailButtonState([
      entry("InvoiceEmailedSent", { outcome: "succeeded", recipient: "a@b.hu" }),
    ]);
    expect(s.kind).toBe("sent");
    expect(s.label_hu).toBe("Újraküldés");
    expect(s.label_en).toBe("Re-send");
    expect(s.glyph).toBe("↻");
  });

  it("failed send → failed ('Újraküldés', ↻)", () => {
    const s = emailButtonState([
      entry("InvoiceEmailedSent", { outcome: "failed", recipient: "a@b.hu" }),
    ]);
    expect(s.kind).toBe("failed");
    expect(s.label_hu).toBe("Újraküldés");
  });

  it("latest send wins — a failure followed by a successful re-send → sent", () => {
    // Entries are append-only; the most-recent InvoiceEmailedSent
    // decides the label so a recovered re-send reads as sent, not failed.
    const s = emailButtonState([
      entry("InvoiceEmailedSent", { outcome: "failed", recipient: "a@b.hu" }),
      entry("InvoiceEmailedSent", { outcome: "succeeded", recipient: "a@b.hu" }),
    ]);
    expect(s.kind).toBe("sent");
  });

  it("empty audit trail → idle", () => {
    expect(emailButtonState([]).kind).toBe("idle");
  });
});
