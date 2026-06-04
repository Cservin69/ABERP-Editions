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
//
// PR-80 / session-102 — `detailActionMeta` + `ActionGroup` extend the
// per-button surface with presentational metadata (glyph + bilingual
// label + group) so the InvoiceDetail action bar renders with
// consistent affordance across lifecycle / operational / chain /
// export sections. The group decision is load-bearing (it drives the
// visual hierarchy on the operator's daily console) so it lives next
// to `buttonsForState` and is pinned by the same test file.

import type { AuditEntryView, InvoiceState } from "./api";

/** Closed vocab of operator-visible action buttons that can appear
 * in the invoice-detail modal header. Kept narrow per CLAUDE.md
 * rule 3 (surgical); a future PR may add `RetrySubmission` /
 * `Recover` / `MarkAbandoned` here when the SPA surfaces those
 * NAV-recovery affordances.
 *
 * PR-95 / session-115 — `PollAck` removed from the union. The
 * new 4-state NAV-status pictogram (`./nav-status-pictogram.ts`)
 * replaces the manual `↻ Lekérés` button: clicking the pictogram
 * when its state is `InFlight` re-polls NAV (same `pollAck` Tauri
 * call). The closed-vocab posture in the action-bar therefore drops
 * the row — the operator's mental model is "the pictogram IS the
 * poll affordance"; surfacing a duplicate button under "NAV státusz"
 * would split the operator's eye between two surfaces. The
 * `pollAck` API function in `./api.ts` is retained verbatim because
 * the pictogram's click handler still invokes it. */
export type DetailActionButton =
  | "Submit"
  | "Storno"
  | "Modification"
  | "Pay"
  | "Download"
  // PR-92 / ADR-0047 — manual SMTP send button. Surfaces only on
  // states where the printed PDF exists (Ready → Finalized → Amended
  // → terminal). The auto-send-after-issue path is independent —
  // this button is operator-on-demand (e.g., to resend after the
  // operator updated the buyer's email).
  | "Email"
  // S239 / PR-233 — pre-allocation draft delete. The detail-modal
  // does NOT render a header button for this (drafts open in
  // IssueInvoice for completion, not in the regular detail page),
  // but the row-level quick-action surfaces it on the InvoiceList.
  // Keeping `Delete` in the shared vocab makes the row-quick-action
  // gate's `Extract<DetailActionButton, ...>` typecheck mechanical.
  | "Delete";

/** Per-state action-button visibility table. Returned in operator-
 * reading order (left-to-right on the modal header); the renderer
 * mounts each one as a quiet button. Pinned by
 * `buttonsForState` table tests in `invoice-actions.test.ts`.
 *
 * PR-70 / ADR-0039 — `paid` second parameter gates the new
 * operational "Pay" button. The button appears ONLY on
 * `state === "Finalized" && !paid` per the brief's explicit rule:
 * unpaid Finalized invoices get the affordance; already-paid
 * Finalized invoices do not; every other state hides the button
 * regardless of payment status (the precondition guard at the
 * backend route layer rejects mark-paid on non-Finalized states
 * with a 409). `paid` defaults to `false` so the unpaid baseline
 * is the default behaviour; existing test fixtures explicitly
 * cover both branches. */
export function buttonsForState(
  state: InvoiceState,
  paid: boolean = false,
): DetailActionButton[] {
  switch (state) {
    case "Ready":
      // Pre-submission: operator can submit, email, or download.
      return ["Submit", "Email", "Download"];
    case "Submitted":
      // Submitted but no terminal ack yet. PR-95 / session-115 —
      // the NAV-status pictogram (clickable when InFlight) is now
      // the poll affordance; the action bar drops the dedicated
      // PollAck button so the operator's eye lands on a single
      // poll surface. Download stays available throughout the
      // lifecycle per A155 + PR-44ε.UI; Email per PR-92.
      //
      // Session 162 — re-surface "Submit" here as a DISABLED in-flight
      // indicator. Ervin's ask (2026-05-29): when the post-issue daemon
      // auto-submits, the detail dialog's "Send to NAV" button looked
      // idle, so the operator could not tell the submit had started.
      // The button renders here only while NAV is processing; the
      // component reads `navSubmitButtonState` to label it
      // "Beküldés folyamatban… / Submitting…" and keep it DISABLED, so
      // it never fires a second `submitInvoice` (the backend would 409
      // a re-submit on a non-`Ready` state per `submit_invoice_request`).
      // On the terminal ack the state leaves `Submitted` and the button
      // drops; re-checking is the pictogram's job, not a resend.
      return ["Submit", "Email", "Download"];
    case "PendingNavExists":
      // PendingNavExists — NAV has the submission but the local ledger
      // lacks the Response/ack pair. Download + Email only; no Submit
      // (re-submit is 409-gated to `Ready`, and the pictogram drives
      // re-poll). Kept separate from `Submitted` per session 162 so the
      // disabled in-flight Submit indicator shows ONLY on the genuine
      // operator-submitted-NAV-processing state.
      return ["Email", "Download"];
    case "Pending":
      // State-2 Pending without Layer-2 evidence: NAV-recovery is the
      // operator's next move (`retry-submission` / `recover-from-nav`).
      // The SPA does not surface those affordances yet — PR-44η scope
      // is the standard lifecycle only. Download stays available.
      return ["Email", "Download"];
    case "Finalized":
      // PR-47α / session-64 — Finalized is the state where storno is
      // legal (ADR-0023 §1: NAV terminal SAVED precondition).
      // PR-47β / session-65 — Finalized is ALSO the base case for
      // modification per ADR-0024 §6 (the `Finalized | Amended`
      // accept set). The backend's `modification_invoice_request`
      // precondition guard mirrors this; surfacing the button on a
      // non-modifiable state would produce a 409 the operator was
      // not warned about.
      //
      // PR-70 / ADR-0039 — Mark-as-paid is gated to Finalized AND
      // unpaid. The button order places "Pay" before the chain
      // operations so the operator-most-common action (recording
      // a payment) sits at the natural left-side button position;
      // a paid Finalized invoice retains only the chain operations.
      // PR-92 / ADR-0047 — Email is always available on a Finalized
      // invoice (the printed PDF + the NAV-accepted state mean the
      // operator may resend on demand).
      if (paid) {
        return ["Storno", "Modification", "Email", "Download"];
      }
      return ["Pay", "Storno", "Modification", "Email", "Download"];
    case "Amended":
      // PR-47β / session-65 — Amended is the second arm of the
      // modify-after-modify accept set (ADR-0024 §6 default-permit
      // posture for chains of modifications). Storno is NOT in this
      // arm: ADR-0024 §6 rejects modification of a stornoed base, and
      // ADR-0023 §1 requires Finalized (SAVED) — an Amended base has
      // no remaining SAVED ack at the top of its chain. The CLI's
      // `issue-storno` would loud-fail at the precondition walker.
      return ["Modification", "Email", "Download"];
    case "Recovered":
    case "Rejected":
    case "Storno":
    case "Abandoned":
    case "Unknown":
      // Terminal / read-only states: download (and email — operator
      // may resend an issued invoice's PDF regardless of NAV terminal
      // state per ADR-0047 §2; e.g. resending a stornoed invoice's
      // PDF to a buyer for their records).
      return ["Email", "Download"];
    case "Draft":
      // S236 / PR-230b — pre-allocation Draft has no NAV number and
      // no PDF; the regular invoice detail page does not load drafts.
      // S239 / PR-233 — surface the Delete affordance for Draft rows
      // so the operator can dismiss a dispatch-spawned draft (e.g.,
      // canceled order) without going through the IssueInvoice flow.
      // The deletion cascades the dispatch's `spawned_invoice_id`
      // pointer in one tx per the S237 §🔴 #1 fix; the
      // confirmation modal explains the consequence per
      // [[hulye-biztos]].
      return ["Delete"];
  }
}

/** PR-80 / session-102 — visual grouping of action bar buttons. Drives
 * the section labels in the InvoiceDetail action bar so the operator
 * scans a coherent hierarchy rather than an ambiguous row of buttons:
 *
 *   Lifecycle  — moves through the NAV regulatory ladder (Submit).
 *                PR-95 dropped the PollAck button from this row; the
 *                NAV-status pictogram now carries the poll affordance.
 *   Operational — operational-side recording that doesn't change the
 *                NAV state (Pay).
 *   Chain      — issues a downstream chain child (Storno, Modification).
 *                Read as "spawns a new invoice referencing this one."
 *   Export     — operator-deliverable artifacts (Download PDF, Email).
 *
 * Closed-vocab so a new ActionGroup variant requires a paired pin. */
export type ActionGroup = "Lifecycle" | "Operational" | "Chain" | "Export";

/** PR-80 / session-102 — bilingual labels per ADR-0036's HU + EN
 * affordance precedent. The visible button shows the Hungarian label
 * (the operator is HU-native; this is the operator's daily console);
 * the English string is the screen-reader aria-label + tooltip
 * fallback so non-HU developers / support contractors can still scan
 * the UI. */
export interface DetailActionMeta {
  /** The action's group bucket, drives section labels. */
  group: ActionGroup;
  /** Leading icon glyph rendered before the label. Mirrors the
   * `quickActionMeta` glyph vocabulary in `invoice-list.ts` so the
   * operator recognises the same affordance on the row + detail
   * surfaces. */
  glyph: string;
  /** Hungarian visible label — operator's native language. */
  label_hu: string;
  /** English label — used as the screen-reader aria-label fallback. */
  label_en: string;
  /** One-sentence tooltip explaining what the action does. Surfaced
   * on hover so a regression that disables the button still tells
   * the operator what would happen. Hungarian per the operator-
   * surface convention. */
  tooltip_hu: string;
}

/** PR-80 / session-102 — per-button metadata table. The visible button
 * renders `glyph + label_hu`; aria-label is `label_en`; tooltip is
 * `tooltip_hu`. Pinned by `detailActionMeta` table tests in
 * `invoice-actions.test.ts` so a glyph drift / label drift surfaces at
 * `npm test`. */
export function detailActionMeta(button: DetailActionButton): DetailActionMeta {
  switch (button) {
    case "Submit":
      return {
        group: "Lifecycle",
        glyph: "↗",
        label_hu: "Beküldés a NAV-hoz",
        label_en: "Submit to NAV",
        tooltip_hu:
          "Beküldi a számlát a NAV Online Számla rendszerébe. A NAV nyugta után az állapot Submitted lesz.",
      };
    case "Pay":
      return {
        group: "Operational",
        glyph: "💰",
        label_hu: "Fizetettnek jelölés",
        label_en: "Mark as paid",
        tooltip_hu:
          "Rögzíti, hogy a vevő kifizette a számlát. Nem változtatja a NAV státuszt.",
      };
    case "Storno":
      return {
        group: "Chain",
        glyph: "⊘",
        label_hu: "Sztornózás",
        label_en: "Cancel (storno)",
        tooltip_hu:
          "Sztornó számlát állít ki az eredeti tartalmával ellentétes előjelű összegekkel. Új sorszámot foglal le; nem visszafordítható.",
      };
    case "Modification":
      return {
        group: "Chain",
        glyph: "✎",
        label_hu: "Módosítás",
        label_en: "Amend (modification)",
        tooltip_hu:
          "Módosító számlát állít ki javított tartalommal. A pénznem örökölt; új sorszámot foglal le.",
      };
    case "Download":
      return {
        group: "Export",
        glyph: "📄",
        label_hu: "PDF letöltése",
        label_en: "Download PDF",
        tooltip_hu:
          "Letölti a nyomtatható PDF számlát. A NAV státusztól függetlenül elérhető.",
      };
    case "Email":
      // PR-92 / ADR-0047 — manual SMTP send. "Email" lands in the
      // Export group alongside Download because both are buyer-
      // deliverable artifact channels (PDF on disk vs PDF in
      // mailbox); the SPA renders them in the same row.
      return {
        group: "Export",
        glyph: "✉",
        label_hu: "Email a vevőnek",
        label_en: "Email to buyer",
        tooltip_hu:
          "Elküldi a számla PDF-jét a vevő e-mail címére az SMTP beállítások szerint. A küldés sikeréről audit napló készül.",
      };
    case "Delete":
      // S239 / PR-233 — pre-allocation draft delete. The detail-modal
      // does not actually render this button today (drafts open in
      // IssueInvoice, not in the regular detail page), but the
      // closed-vocab `DetailActionButton` requires an exhaustive arm
      // so the metadata is here for future-proofing + so the
      // InvoiceList row-quick-action's metadata composition can stay
      // mechanical. Group is "Operational" — the same group as Pay
      // (destructive operator decision against the row's life).
      return {
        group: "Operational",
        glyph: "🗑",
        label_hu: "Piszkozat törlése",
        label_en: "Delete draft",
        tooltip_hu:
          "Véglegesen törli a piszkozatot. Ha kiszállításhoz kapcsolódott, a kiszállítás “spawn link”-je egy tranzakcióban megszűnik.",
      };
  }
}

/** PR-80 / session-102 — group buttons returned by `buttonsForState`
 * into the four visual sections the action bar renders. Preserves the
 * per-state operator-reading order within each group. Returns groups
 * with at least one button (empty groups are omitted so the rendered
 * bar has no orphan section labels). Pinned by table tests so a group
 * drift / order drift surfaces at `npm test`.
 *
 * Returned shape: array of `{ group, buttons }` in the canonical group
 * order (Lifecycle → Operational → Chain → Export) so the operator's
 * eye lands on the NAV-ladder actions first, then operational,
 * then chain, then export. */
export function groupButtons(
  buttons: DetailActionButton[],
): { group: ActionGroup; buttons: DetailActionButton[] }[] {
  const order: ActionGroup[] = ["Lifecycle", "Operational", "Chain", "Export"];
  const grouped: Map<ActionGroup, DetailActionButton[]> = new Map();
  for (const button of buttons) {
    const { group } = detailActionMeta(button);
    const existing = grouped.get(group) ?? [];
    existing.push(button);
    grouped.set(group, existing);
  }
  return order
    .filter((g) => grouped.has(g))
    .map((g) => ({ group: g, buttons: grouped.get(g)! }));
}

/** PR-80 / session-102 — Hungarian section labels rendered above each
 * group of buttons. Pinned so a label drift surfaces. */
export function actionGroupLabel(group: ActionGroup): {
  label_hu: string;
  label_en: string;
} {
  switch (group) {
    case "Lifecycle":
      return { label_hu: "NAV státusz", label_en: "NAV lifecycle" };
    case "Operational":
      return { label_hu: "Művelet", label_en: "Operational" };
    case "Chain":
      return { label_hu: "Számlalánc", label_en: "Invoice chain" };
    case "Export":
      return { label_hu: "Export", label_en: "Export" };
  }
}

// ── Session 162 — audit-driven button state for the detail dialog ────
//
// Ervin's ask (2026-05-29): when the operator issues with auto-submit-
// to-NAV + auto-email toggled on, the detail dialog opens (S158) and the
// post-issue daemon (S158 + S161) fires those actions in the background.
// The "Send to NAV" / "Send email" buttons showed their idle labels, so
// the operator could not tell anything had started. These helpers derive
// the button's label/affordance from the live audit ledger (the same
// audit-immutable, derive-never-write pattern that powers the pictogram).
//
// Audit-vocabulary reality (verified against
// `crates/audit-ledger/.../event_kind.rs`, NOT the brief's guessed
// names): NAV submit writes `InvoiceSubmissionAttempt` (before the POST
// returns), then `InvoiceSubmissionResponse` + `InvoiceAckStatus`
// (ack_status RECEIVED/PROCESSING/SAVED/ABORTED), or
// `InvoiceSubmissionAttemptFailed` on a transport-class failure. Email
// writes exactly ONE `InvoiceEmailedSent` per send attempt (payload
// `outcome: "succeeded" | "failed"`) — there is NO queued/started event,
// so email "in flight" is NOT observable from the ledger (see the
// `emailButtonState` doc comment).

/** Closed vocab of the NAV-submit button's audit-derived states.
 * `not_submitted` → operator hasn't submitted; `in_flight` → submitted,
 * NAV processing (no terminal ack yet); `saved` → NAV accepted (SAVED);
 * `failed` → NAV rejected (ABORTED) or the attempt failed at transport.
 *
 * The action bar renders the NAV-submit button ONLY on `Ready`
 * (`not_submitted`) and the in-flight `Submitted` state (`in_flight`,
 * shown DISABLED). The `saved` / `failed` arms are derivation-complete
 * and pinned, but NOT surfaced as a button: re-submit is 409-gated to
 * `Ready` at `serve::submit_invoice_request`, so a "Resend to NAV"
 * affordance would always error — re-checking a terminal invoice is the
 * NAV-status pictogram's job, not a resubmit. */
export type NavSubmitButtonKind =
  | "not_submitted"
  | "in_flight"
  | "saved"
  | "failed";

export interface NavSubmitButtonState {
  kind: NavSubmitButtonKind;
  /** Hungarian visible label — operator's native language. */
  label_hu: string;
  /** English label — screen-reader aria-label fallback. */
  label_en: string;
  /** Leading glyph. `…` for in-flight (paired with a CSS spinner). */
  glyph: string;
  /** Whether the button is disabled. True for `in_flight` (the operator
   * must not double-submit while NAV is processing) and the two terminal
   * arms (no legal re-submit). False only for `not_submitted`. */
  disabled: boolean;
}

/** Read the `ack_status` string off an `InvoiceAckStatus` payload.
 * Mirrors `invoice-timeline.ts::readAckStatus`; narrows defensively so a
 * malformed payload reads as `null` (no terminal) rather than crashing. */
function readAckStatus(payload: unknown): string | null {
  if (typeof payload !== "object" || payload === null) return null;
  const ack = (payload as { ack_status?: unknown }).ack_status;
  return typeof ack === "string" ? ack : null;
}

/** Derive the NAV-submit button's audit state from an invoice's audit
 * entries (any order — the predicates are presence-based, not
 * positional). Precedence: a SAVED ack wins (terminal positive); else an
 * ABORTED ack or a transport-class `InvoiceSubmissionAttemptFailed` is
 * terminal negative; else a bare `InvoiceSubmissionAttempt` is in flight;
 * else nothing has been submitted. Pinned by `invoice-actions.test.ts`. */
export function navSubmitButtonState(
  entries: AuditEntryView[],
): NavSubmitButtonState {
  let hasAttempt = false;
  let hasSaved = false;
  let hasFailed = false;
  for (const e of entries) {
    if (e.kind === "InvoiceSubmissionAttempt") hasAttempt = true;
    else if (e.kind === "InvoiceSubmissionAttemptFailed") hasFailed = true;
    else if (e.kind === "InvoiceAckStatus") {
      const ack = readAckStatus(e.payload);
      if (ack === "SAVED") hasSaved = true;
      else if (ack === "ABORTED") hasFailed = true;
    }
  }
  if (hasSaved) {
    return {
      kind: "saved",
      label_hu: "Beküldve",
      label_en: "Submitted to NAV",
      glyph: "✓",
      disabled: true,
    };
  }
  if (hasFailed) {
    return {
      kind: "failed",
      label_hu: "Beküldés sikertelen",
      label_en: "Submission failed",
      glyph: "⚠",
      disabled: true,
    };
  }
  if (hasAttempt) {
    return {
      kind: "in_flight",
      label_hu: "Beküldés folyamatban…",
      label_en: "Submitting…",
      glyph: "…",
      disabled: true,
    };
  }
  return {
    kind: "not_submitted",
    label_hu: "Beküldés a NAV-hoz",
    label_en: "Submit to NAV",
    glyph: "↗",
    disabled: false,
  };
}

/** Closed vocab of the email button's audit-derived states. `idle` → no
 * send recorded yet (first-time affordance); `sent` → the latest send
 * succeeded (re-send affordance + the operator sees the "Elküldve {time}"
 * pill the component derives separately); `failed` → the latest send
 * failed (re-send affordance + an error marker).
 *
 * NO `in_flight` arm: the ledger writes exactly one `InvoiceEmailedSent`
 * per send ATTEMPT (success OR failure) and has NO queued/started event,
 * so an in-flight send is not observable from the audit ledger. The
 * manual-click in-flight is surfaced by the component's `mutationState`
 * ('emailing'); the post-issue daemon's auto-send in-flight has no
 * observable signal and is deliberately NOT faked (CLAUDE.md rule 12). */
export type EmailButtonKind = "idle" | "sent" | "failed";

export interface EmailButtonState {
  kind: EmailButtonKind;
  label_hu: string;
  label_en: string;
  glyph: string;
}

/** Read the `outcome` string off an `InvoiceEmailedSent` payload. */
function readEmailOutcome(payload: unknown): string | null {
  if (typeof payload !== "object" || payload === null) return null;
  const outcome = (payload as { outcome?: unknown }).outcome;
  return typeof outcome === "string" ? outcome : null;
}

/** Derive the email button's audit state from an invoice's audit
 * entries. The LATEST `InvoiceEmailedSent` (highest seq — entries are
 * append-only, so the last matching entry) decides the label: a prior
 * failed send followed by a successful re-send reads as `sent`. Pinned by
 * `invoice-actions.test.ts`. */
export function emailButtonState(entries: AuditEntryView[]): EmailButtonState {
  let latestOutcome: string | null = null;
  for (const e of entries) {
    if (e.kind === "InvoiceEmailedSent") {
      const outcome = readEmailOutcome(e.payload);
      if (outcome !== null) latestOutcome = outcome;
    }
  }
  if (latestOutcome === "succeeded") {
    return {
      kind: "sent",
      label_hu: "Újraküldés",
      label_en: "Re-send",
      glyph: "↻",
    };
  }
  if (latestOutcome === "failed") {
    return {
      kind: "failed",
      label_hu: "Újraküldés",
      label_en: "Re-send",
      glyph: "↻",
    };
  }
  return {
    kind: "idle",
    label_hu: "Email a vevőnek",
    label_en: "Email to buyer",
    glyph: "✉",
  };
}
