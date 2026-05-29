<script lang="ts">
  // PR-25 / session-29 — Invoice-detail modal.
  //
  // Renders one invoice's metadata plus its full audit-ledger trail in
  // a native <dialog>. Mounted once at the App level; opens / closes
  // by `invoiceId` prop toggling between a string and `null`. ADR-0021
  // §Part B's wire surface is reused via `getInvoice` — no new Tauri
  // command. Per ADR-0036 §7 the state chip reuses `labels.ts` so the
  // detail header and the list row carry identical affordances; per
  // ADR-0017 the audit-entries table uses the same dense pattern as
  // InvoiceList (monospace, tabular numbers, hairline dividers, no
  // chrome). No SvelteKit routing dependency added — modal posture
  // matches CLAUDE.md rule 3.
  //
  // Why a native <dialog>: the browser handles focus trap, ESC
  // dismiss, inert-on-backdrop, ARIA modal semantics, and stacking
  // context. A custom modal component would re-implement five things
  // that already exist. Per CLAUDE.md rule 2 — simplicity first.
  //
  // PR-26 / session-30 — chain-link clickable navigation. The Rust
  // side now emits `chain_base_invoice_id: Option<String>` on the
  // `AuditEntryView` shape (typed payload probe over
  // `InvoiceStornoIssued` / `InvoiceModificationIssued` entries).
  // The kind cell for a chain-link row renders `<kind> → <base_id>`
  // where the base id is a button that calls `onNavigate(baseId)`;
  // the parent rebinds the modal's `invoiceId` prop and the existing
  // `$effect` fetches the base invoice's data into the SAME dialog
  // (no breadcrumb stack — operator's browser-Back-equivalent is
  // their head per the session-29 handoff lean). No new audit event
  // fires on navigation — inspection is read-only per CLAUDE.md
  // rule 13.
  //
  // PR-27 / session-31 — audit-entry payload drill-down. The Rust
  // side now emits `payload: serde_json::Value` on the
  // `AuditEntryView` shape (the full typed payload bytes parsed back
  // as raw JSON; the audit_payloads.rs F9 discipline guarantees
  // valid JSON). Each row gets a small `▸ / ▾` expand button at the
  // start of the kind cell; toggling reveals a colspan-4 sub-row
  // with the pretty-printed JSON payload underneath. No new Tauri
  // command, no new audit event — the same `getInvoice` round-trip
  // already carries the field. Expansion state is per-modal-mount
  // (a fresh open clears the set), matching the chain-link
  // navigation posture of treating the modal as a single inspection
  // context. Per the PR-27 lean: no redaction default (matches
  // `aberp dump-audit-bundle` posture); F43 bundle-redaction
  // posture stays named-deferred.
  //
  // PR-29 / session-33 — bytes-as-UTF-8 reviver. `audit_payloads::*`
  // carries the NAV XML envelopes (`request_xml`, `response_xml`,
  // `ack_xml`, and `Option<Vec<u8>>` failure / annulment variants)
  // as `Vec<u8>` fields. Serde's default emission renders those as
  // JSON int arrays — readable in principle, useless in practice
  // (an inspector saw `[60, 63, 120, ...]` rather than
  // `<ManageInvoiceRequest>...`). `formatPayload` now routes the
  // payload through `bytesAsUtf8Replacer` from
  // `../lib/payload-reviver`, which substitutes any UTF-8-decodable
  // byte-array subtree with its decoded string. Non-UTF-8 arrays
  // pass through unchanged per CLAUDE.md rule 12 (fail loud rather
  // than render U+FFFD garbage). SPA-side only: no Rust change, no
  // new Tauri command, no new audit event, no new SPA dependency,
  // no new TS interface field. Surfaced by the session-31 close
  // handoff's named Option N trigger.
  //
  // PR-30 / session-34 — breadcrumb / back-button navigation
  // (Option L). The pre-PR-30 chain-link traversal (PR-26) rebound
  // the modal's `invoiceId` prop in place and lost the navigation
  // history — an operator three storno-chains deep could not get
  // back without remembering ids. The stack now lives on the
  // parent (`InvoiceList.svelte` owns `navStack: string[]`); this
  // modal stays presentational and renders the trail. `ancestors`
  // is the slice of the stack below the top (the entries the
  // operator walked through to get here); the current invoice is
  // `invoiceId` as before. Each ancestor renders as a quiet
  // `← {id}` button in the header; clicking pops the stack back to
  // that level via `onJumpBack(index)`. ESC / backdrop close
  // clears the whole stack (matches the modal-as-single-
  // inspection-context posture from PR-26 / PR-27 / PR-29). No
  // new audit event — inspection remains read-only per CLAUDE.md
  // rule 13.
  //
  // PR-32 / session-36 — chain-children list on the BASE's detail
  // view (Option T). PR-31 surfaced "this row has chain children"
  // at the list-row badge layer; this PR answers "WHICH children"
  // inside the modal. The backend's `get_invoice_detail` walker
  // collects every `InvoiceStornoIssued` / `InvoiceModificationIssued`
  // entry whose `base_invoice_id` equals the queried invoice and
  // emits the chain child's own id under a typed `chain_children:
  // ChainChildView[]` wire field. The renderer mounts a section
  // between the meta-grid and the audit-trail table, one row per
  // child (`<kind> → <invoice_id>`); each invoice_id reuses the
  // `onNavigate` callback that PR-26 wired for audit-row chain-
  // link buttons, so the operator can step in either direction of
  // the chain (child → base via the audit-row link, base → child
  // via this new list). No new Tauri command, no new audit event
  // — inspection remains read-only per CLAUDE.md rule 13.
  //
  // PR-33 / session-37 — typed wire mirror of the latest NAV ack
  // (Option Q). The backend's `get_invoice_detail` now emits a
  // typed `last_ack_status: AckStatus | null` field; the renderer
  // surfaces the value as a fifth meta-grid row beneath State /
  // Total (gross). `null` renders as `—` (matches the
  // `total_gross` null-render posture from PR-25). The value is
  // the most-recent NAV ack for the invoice — useful for the
  // RECEIVED / PROCESSING intermediate states the `Submitted`
  // state chip collapses (the chip discriminates only the
  // terminal SAVED / ABORTED ack values via `Finalized` /
  // `Rejected`). Continues the typed-enum precedent from PR-28
  // (`InvoiceState`) and PR-32 (`ChainChildKind`). No new Tauri
  // command, no new audit event — inspection remains read-only
  // per CLAUDE.md rule 13.
  //
  // PR-34 / session-38 — kind-label dispatch on the chain-children
  // list (Option V). The PR-32 chain-children section rendered each
  // row's kind as plain mono text (`<span class="chain-child-kind">
  // Storno</span>`); this PR routes the kind through `labelMeta(...)`
  // from `labels.ts` so the row carries the same icon + signal
  // colour + tooltip as the state chip in the meta-grid above. The
  // `ChainChildKind` typed union ("Storno" | "Amended") is a strict
  // subset of `InvoiceState` so `labelMeta` resolves to a known
  // entry on every wire value (`⊘` warning for Storno, `✎` warning
  // for Amended); the muted "?" fallback per CLAUDE.md rule 12
  // remains as a guardrail if the backend ever invents a kind the
  // SPA does not model. No wire-shape change, no api.ts change, no
  // labels.ts change, no new SPA dependency. Reuses the existing
  // `.state-pill` CSS plus the per-signal classes; the now-unused
  // `.chain-child-kind` rule is removed (its `color:
  // var(--color-text-secondary)` lived only inside this list and is
  // superseded by the per-signal-class colouring on the pill).
  //
  // PR-36 / session-40 — ack-pill render (Option Y). The PR-33
  // "Latest ack" meta-grid cell rendered the typed `last_ack_status`
  // wire field as plain mono text (`{detail.last_ack_status ?? "—"}`);
  // this PR routes the value through `ackLabelMeta(...)` from
  // `labels.ts` so the cell carries the same icon + signal colour +
  // tooltip as the State chip directly above. `null` continues to
  // render as a plain `—` (no pill — there is no value to label, and
  // that matches the `total_gross` null-render posture). The new
  // `ACK_LABELS` table is a fork (sibling table next to `LABELS`),
  // not a widening, because `AckStatus` and `InvoiceState` are
  // disjoint concept domains — see the design comment in labels.ts.
  // Reuses the existing `.state-pill` CSS plus the per-signal
  // classes. Completes the detail modal's label-rendering
  // consistency: every label-typed cell in the modal (State, chain-
  // children kind, Latest ack) now renders as the same labelMeta
  // chip. No wire-shape change, no api.ts change, no new SPA
  // dependency.
  //
  // PR-41 / session-45 — `modification_index` on chain-children rows
  // (Option W). The PR-32 chain-children section emitted one row per
  // child (`<kind> → <invoice_id>`) but did NOT carry the per-base
  // chain index. The backend's `extract_chain_link` probe pulls the
  // value off `InvoiceStornoIssuedPayload.modification_index` /
  // `InvoiceModificationIssuedPayload.modification_index` (shared name
  // space per `next_modification_index_in_tx`); the renderer surfaces
  // it as a leading `#N` mono glyph on each row so the operator can
  // cross-reference the per-row index against the NAV-side
  // `<modificationIndex>` that the storno / modification XML emits.
  // No wire-shape change beyond the one new field, no labels.ts
  // change, no new SPA dependency. Reuses the existing
  // `.chain-children-list` row layout; the new `.chain-index-prefix`
  // span carries the quiet mono `#N` glyph in the same secondary text
  // colour as `.chain-arrow`.


  import {
    cancelInvoiceStorno,
    downloadInvoicePdf,
    emailInvoiceToBuyer,
    getInvoice,
    markInvoicePaid,
    parseAlreadyPaidError,
    parseNavUpstreamFault,
    pollAck,
    submitInvoice,
    type BankAccountSnapshot,
    type Currency,
    type InvoiceDetail,
    type MarkPaidRequest,
    type NavUpstreamFault,
    type PaymentMethod,
  } from "../lib/api";
  import {
    ackLabelMeta,
    labelMeta,
    type LabelSignal,
  } from "../lib/labels";
  import { onDestroy } from "svelte";
  import { navStatusPictogram } from "../lib/nav-status-pictogram";
  import {
    createDetailPoller,
    isPollTerminal,
    type DetailPoller,
    type DetailPollSnapshot,
  } from "../lib/invoice-detail-poll";
  import {
    actionGroupLabel,
    buttonsForState,
    detailActionMeta,
    groupButtons,
    type DetailActionButton,
  } from "../lib/invoice-actions";
  import {
    filenameForInvoice,
    formatHufEquivalent,
    formatInvoiceDate,
    formatInvoiceTotal,
    formatRate,
    formatRateDate,
    formatTotal,
  } from "../lib/format";
  import { bytesAsUtf8Replacer } from "../lib/payload-reviver";
  import InvoiceTimeline from "../lib/InvoiceTimeline.svelte";
  import { timelineFromAuditEntries } from "../lib/invoice-timeline";

  interface Props {
    invoiceId: string | null;
    /** PR-30 — entries below the current invoice in the parent's
     * navigation stack. Empty when the modal is opened directly
     * from the list; one entry per chain-link traversal taken since.
     * Index 0 is the root of the trail (the invoice the operator
     * originally opened); the last entry is the immediate parent of
     * `invoiceId`. */
    ancestors: string[];
    onClose: () => void;
    /** PR-26 — chain-link navigation callback. Invoked when the
     * operator clicks the base invoice id rendered next to an
     * `InvoiceStornoIssued` / `InvoiceModificationIssued` audit row.
     * PR-30: the parent pushes the base id onto `navStack` and the
     * `$effect` below re-fetches into the same modal. */
    onNavigate: (baseId: string) => void;
    /** PR-30 — breadcrumb jump-back callback. The parent slices its
     * `navStack` to length `index + 1`, dropping every entry beyond
     * the clicked ancestor. The clicked ancestor becomes the new
     * top of the stack and the `$effect` below re-fetches into the
     * same modal. */
    onJumpBack: (index: number) => void;
    /** PR-47β / session-65 — Modification button callback. Invoked
     * when the operator clicks "Amend invoice (modification)" on a
     * `Finalized` or `Amended` base. The parent opens the
     * `ModificationInvoice` modal pre-filled from the base; the new
     * modification's id is then surfaced via the modal's own
     * `onAmended` and the parent navigates the detail modal to the
     * chain child. */
    onAmend: (
      baseInvoiceId: string,
      baseCurrency: Currency,
      baseInvoiceNumber: string,
      baseBankAccount: BankAccountSnapshot | null,
    ) => void;
    /** ADR-0049 §Initiation (session 156) — when `true`, the modal
     * auto-opens the inline storno confirm panel once `invoiceId`'s
     * detail loads (and only if the invoice is storno-eligible). This is
     * the row quick-action's entry point: `InvoiceList.svelte::
     * triggerRowStorno` sets this instead of firing its own (Tauri-
     * unreliable) `window.confirm`, routing the operator into the one
     * canonical confirm+reason surface. One-shot: consumed on the first
     * load after open; the parent resets it on close / chain-navigation.
     * Defaults to `false` (the normal "open to inspect" path). */
    openStornoOnLoad?: boolean;
  }

  let {
    invoiceId,
    ancestors,
    onClose,
    onNavigate,
    onJumpBack,
    onAmend,
    openStornoOnLoad = false,
  }: Props = $props();

  // ADR-0049 §Initiation (session 156) — one-shot latch for the row
  // quick-action's auto-open. Armed from `openStornoOnLoad` when the
  // modal opens (the `invoiceId` open-effect below); consumed by `load`
  // on the first successful fetch so a subsequent refetch (post-storno,
  // post-pay) or chain-navigation does NOT re-open the panel.
  let stornoAutoOpenPending: boolean = $state(false);

  let dialogEl: HTMLDialogElement | null = $state(null);
  let detail: InvoiceDetail | null = $state(null);
  let loadState: "idle" | "loading" | "loaded" | "error" = $state("idle");
  let errorMessage: string | null = $state(null);
  // PR-44ε.UI / session-58 — "Download PDF" button state. The button
  // is visible whenever a detail has loaded (HUF + EUR alike per the
  // session-58 brief — PR-44ε.1 handles both at the render layer).
  // `idle` while waiting for an operator click; `downloading` while
  // the Tauri command is in flight (disables the button and shows
  // an inline spinner glyph); `error` to render the inline failure
  // message in the dialog header below the button. No toast
  // component is introduced per CLAUDE.md rule 13.
  let downloadState: "idle" | "downloading" | "error" = $state("idle");
  let downloadError: string | null = $state(null);
  // PR-44η / session-60 — "Submit to NAV" + "Poll ack now" buttons.
  // Shared `mutationState` discriminator so the modal header only ever
  // shows one inline-error pane at a time (the operator can only click
  // one button per modal-open window per CLAUDE.md rule 12 — surfacing
  // two simultaneous errors would muddy the cause-of-failure).
  // `idle` while waiting for an operator click; `submitting` /
  // `polling` while the corresponding Tauri command is in flight
  // (disables every action button and shows an inline spinner glyph
  // on the active one); `error` to render the inline failure message
  // in the dialog header below the buttons.
  // PR-47α / session-64 — `cancelling` extends the discriminator for
  // the storno button. Shares the `mutationState` slot so only one
  // mutation banner ever renders at once (same rationale as PR-44η).
  // PR-58 / session-78 — the error variant carries an optional parsed
  // `NavUpstreamFault` payload. When the backend rejects with HTTP 502
  // and the typed `nav_upstream_fault` body, we display the parsed
  // `fault_code` + `fault_message` prominently (operator-actionable)
  // instead of the opaque "internal error" string. `message` remains
  // the fallback so non-fault errors render unchanged.
  type MutationState =
    | { kind: "idle" }
    | { kind: "submitting" }
    | { kind: "polling" }
    | { kind: "cancelling" }
    | { kind: "paying" }
    | { kind: "emailing" }
    | {
        kind: "error";
        action: "submit" | "poll" | "cancel" | "pay" | "email";
        message: string;
        navFault: NavUpstreamFault | null;
      };
  let mutationState: MutationState = $state({ kind: "idle" });

  // PR-92 / ADR-0047 — surface the last email send outcome inline so
  // the operator sees succeeded/failed + recipient + error class.
  // Rendered as a status banner under the action bar; cleared on a
  // fresh email-button click. Also visible in the audit log table —
  // this state is a fast operator-feedback path on top of the durable
  // audit-ledger record.
  let lastEmailOutcome: import("../lib/api").EmailRouteOutcome | null =
    $state(null);

  // PR-99 Item 4 Part A — derive the most-recent successful
  // InvoiceEmailedSent audit entry so the action bar can swap the
  // "Email" affordance for a "↻ Újraküldés / Re-send" button + show a
  // "✉ Elküldve HH:MM" status. Pre-PR-99 the operator issued with the
  // default-on email toggle, the auto-send fired (audit row written),
  // but the detail view's action bar still showed "Email a vevőnek"
  // as a primary affordance — implying nothing had been sent. The
  // derived shape carries the wall-clock time of the latest send and
  // the recipient line. Failed sends are NOT included here (operator
  // sees them via `lastEmailOutcome` after a manual click + via the
  // audit table); a failure does not block re-sending.
  let lastSuccessfulEmail = $derived.by(() => {
    if (!detail) return null;
    // Walk in reverse (audit_entries is monotonic on seq, so the
    // most-recent success is the last matching entry).
    for (let i = detail.audit_entries.length - 1; i >= 0; i -= 1) {
      const entry = detail.audit_entries[i];
      if (entry.kind !== "InvoiceEmailedSent") continue;
      const p = entry.payload as { outcome?: unknown; recipient?: unknown };
      if (p && p.outcome === "succeeded" && typeof p.recipient === "string") {
        return { occurredAt: entry.occurred_at, recipient: p.recipient };
      }
    }
    return null;
  });

  function formatEmailSentTime(rfc3339: string): string {
    // RFC3339 → "HH:MM" wall-clock in the operator's local zone.
    // Falls back to the raw string on parse failure so we never hide
    // the audit data behind an opaque dash.
    const d = new Date(rfc3339);
    if (Number.isNaN(d.getTime())) return rfc3339;
    const hh = String(d.getHours()).padStart(2, "0");
    const mm = String(d.getMinutes()).padStart(2, "0");
    return `${hh}:${mm}`;
  }

  // PR-80 / session-102 — inline storno confirm panel. The pre-PR-80
  // posture (PR-47α) used a browser-native `window.confirm()` which
  // pops a modal-over-modal that visually disconnects the confirmation
  // from the invoice being cancelled (the operator sees a tiny dialog
  // and a long block of explanatory text, not the invoice they're
  // about to storno). PR-80 elevates this to an in-place panel within
  // the action bar that surfaces the invoice's identifying fields
  // alongside the explanatory copy.
  //
  // PR-83 / session-103 — buyer-facing storno reason. The reason is
  // the operator's "here's why I cancelled" note to the buyer; it
  // lands on the storno's printed PDF / email body so the buyer reads
  // it on the same document. Bound to `stornoReason` below; trimmed +
  // normalised empty-to-null at the wire boundary by the backend
  // route (matches PR-82's blankToNull posture). NEVER carried into
  // the NAV XML — see ADR-0042. Bilingual label per Ervin's
  // HU/EN labelling convention; hint copy makes the recipient-facing
  // surface obvious to the operator.
  let stornoConfirmOpen: boolean = $state(false);
  let stornoReason: string = $state("");

  // PR-70 / ADR-0039 — mark-as-paid modal state. The modal is a
  // small inline <dialog> driven by `paymentDialogOpen` rather than
  // a full route component (the form is four fields + a submit
  // button; a dedicated route would be overkill per CLAUDE.md
  // rule 2). The form state lives here so the modal mounts and
  // unmounts cleanly with the parent InvoiceDetail modal.
  let paymentDialogOpen: boolean = $state(false);
  let paymentDialogEl: HTMLDialogElement | null = $state(null);
  // Form fields. Defaults are populated when the operator clicks
  // the "Mark as paid" button (`triggerOpenMarkPaid`) so today's
  // date and the invoice's total_gross / currency are pre-filled
  // — the operator only needs to pick a method and (optionally)
  // type a reference for the common case.
  let payForm: {
    paid_at: string;
    amount_minor: string; // string for the <input type="number"> binding
    method: PaymentMethod;
    reference: string;
  } = $state({
    paid_at: "",
    amount_minor: "",
    method: "BankTransfer",
    reference: "",
  });
  // Inline error message for the modal form. Populated when the
  // backend rejects the POST (400 invalid date, 409 already paid,
  // etc.); cleared on every fresh submit attempt.
  let payFormError: string | null = $state(null);
  // PR-27 — per-row expanded-payload state. Reassignment pattern
  // (build a new Set on every toggle) guarantees Svelte 5
  // reactivity without depending on Set-mutation tracking through
  // the $state proxy. The set is keyed by `entry.seq` because seq
  // is the audit-ledger's append-only primary key per ADR-0008 —
  // unique per ledger AND stable across the lifetime of the modal.
  let expandedSeqs: Set<number> = $state(new Set());
  // PR-67 / session-89 — "Show raw table" toggle for the audit
  // trail. The pre-PR-67 detail modal rendered the audit entries
  // as a dense seq/kind/actor/occurred_at table; PR-67 swaps the
  // default to the visual lifecycle timeline (operator-meaningful
  // narrative) but preserves the table behind this toggle for
  // power-user inspection (payload drill-down, chain navigation
  // affordance, raw kind strings). Per-modal-mount state only —
  // no localStorage per the brief's "persisted only in component
  // state" rule; a fresh modal open resets to timeline-visible.
  let showRawTable: boolean = $state(false);

  // ── Session 158 — live audit-entries poll ────────────────────────
  // Ervin's flow: Issue → navigate straight here → watch the NAV
  // pictogram + email status progress LIVE as the post-issue background
  // tail (auto-submit → poll → SAVED/ABORTED, plus the auto-send-to-
  // buyer email) lands its audit-ledger entries. The pictogram (PR-95)
  // and the email-sent status (PR-99 `lastSuccessfulEmail`) already
  // derive reactively from `detail.audit_entries`; this loop simply
  // re-fetches `detail` every ~2s while the invoice is still in flight
  // so those derivations see fresh data without an operator click.
  //
  // The loop's brain (terminal predicates + interval lifecycle + the
  // 5-minute cap) lives in `lib/invoice-detail-poll.ts` as injectable
  // pure functions so vitest pins every branch; this component supplies
  // the real refetch (a QUIET reload that updates `detail` in place,
  // without the loading-spinner flicker `load` would cause) and the
  // real timer/clock. `start()` is called from `load` only when the
  // freshly-loaded invoice is non-terminal (an already-SAVED invoice
  // opened from the list does one fetch, no interval); `stop()` fires
  // on navigation, on close, and on destroy so no zombie interval
  // outlives the modal.
  function detailSnapshot(d: InvoiceDetail): DetailPollSnapshot {
    return {
      navState: navStatusPictogram(d.state).state,
      hasEmailAttempt: d.audit_entries.some(
        (e) => e.kind === "InvoiceEmailedSent",
      ),
    };
  }

  const poller: DetailPoller = createDetailPoller({
    refetch: async () => {
      const id = detail?.invoice_id ?? invoiceId;
      if (id === null || id === undefined) {
        // Nothing to poll (modal closed mid-flight). Return a settled
        // snapshot so the loop stops cleanly; `stop()` will also have
        // fired from the close branch.
        return { navState: "Final", hasEmailAttempt: true };
      }
      const fresh = await getInvoice(id);
      detail = fresh;
      return detailSnapshot(fresh);
    },
    onCapExceeded: () => {
      console.warn(
        "InvoiceDetail live poll hit the 5-minute cap without a terminal " +
          "state; stopping. The operator can refresh manually or click the " +
          "actionable pictogram.",
      );
    },
    onError: (err) => {
      // Tolerate a transient local fetch blip — keep the last-good
      // detail on screen and retry on the next tick.
      console.warn("InvoiceDetail live poll refetch failed (will retry)", err);
    },
    now: () => Date.now(),
    setIntervalFn: (cb, ms) => setInterval(cb, ms),
    clearIntervalFn: (handle) => clearInterval(handle),
  });

  // No zombie interval if the operator navigates away (the parent
  // unmounts this component when the dialog's `close` resets the
  // navStack).
  onDestroy(() => poller.stop());

  // Drive the dialog open/close lifecycle from the `invoiceId` prop.
  // Opening: invoke `showModal()` and kick off the fetch. Closing:
  // invoke `close()` if the dialog is still open. Guarded against the
  // double-open `InvalidStateError` from the platform.
  $effect(() => {
    if (!dialogEl) return;
    if (invoiceId !== null) {
      if (!dialogEl.open) dialogEl.showModal();
      // PR-27 — reset expansion state when navigating to a new
      // invoice (whether via fresh open or chain-link navigation).
      // The seq numbers from one invoice's audit lineage are
      // unrelated to another's, so a stale set would leak
      // expansion state across inspection contexts.
      expandedSeqs = new Set();
      // PR-67 — reset the raw-table toggle on every navigation so
      // the operator's chain-link traversal starts at the timeline
      // (the default narrative view), not whichever toggle state
      // a prior modal left.
      showRawTable = false;
      // PR-44ε.UI — reset the download state on every navigation so
      // a stale error from a prior invoice doesn't leak into the
      // new inspection context.
      downloadState = "idle";
      downloadError = null;
      mutationState = { kind: "idle" };
      // PR-70 / ADR-0039 — drop any open Pay dialog on navigation
      // (the form's defaults are bound to the previous invoice's
      // currency + total).
      paymentDialogOpen = false;
      payFormError = null;
      // PR-80 / session-102 — close the inline storno confirm panel
      // on navigation so a half-typed confirm doesn't leak into the
      // next inspection context.
      stornoConfirmOpen = false;
      stornoReason = "";
      // ADR-0049 §Initiation — arm the row quick-action's auto-open
      // latch from the prop. `load` consumes it on success (one-shot),
      // so a later refetch in the same modal does not re-open the panel.
      stornoAutoOpenPending = openStornoOnLoad === true;
      // Session 158 — stop any live poll from the PRIOR invoice before
      // the chain-navigation fetch starts, so an in-flight tick can't
      // race the new load. `load` (re)starts it once the new detail
      // lands and proves non-terminal.
      poller.stop();
      void load(invoiceId);
    } else {
      if (dialogEl.open) dialogEl.close();
      detail = null;
      loadState = "idle";
      errorMessage = null;
      expandedSeqs = new Set();
      showRawTable = false;
      downloadState = "idle";
      downloadError = null;
      mutationState = { kind: "idle" };
      paymentDialogOpen = false;
      payFormError = null;
      stornoConfirmOpen = false;
      stornoReason = "";
      // Session 158 — modal closed; tear down the live poll.
      poller.stop();
    }
  });

  async function load(id: string) {
    loadState = "loading";
    errorMessage = null;
    detail = null;
    try {
      detail = await getInvoice(id);
      loadState = "loaded";
      // ADR-0049 §Initiation (session 156) — consume the row quick-
      // action's one-shot auto-open latch. Open the inline storno
      // confirm panel only if the loaded invoice is actually storno-
      // eligible (the same `buttonsForState` gate the action bar uses);
      // a non-Finalized invoice has no Storno button, so arming it would
      // surface a panel for an action the backend would 409. Resetting
      // the latch here means a subsequent refetch (post-storno, post-pay)
      // never re-opens the panel.
      if (stornoAutoOpenPending) {
        stornoAutoOpenPending = false;
        const eligible = buttonsForState(
          detail.state,
          detail.payment !== null,
        ).includes("Storno");
        if (eligible) {
          stornoReason = "";
          stornoConfirmOpen = true;
        }
      }
      // Session 158 — (re)start the live poll iff this invoice is still
      // in flight. Stop first so a re-load (chain navigation, or a
      // post-mutation refetch from triggerSubmit / triggerEmail) never
      // stacks a second interval; then start only when the fresh detail
      // is non-terminal. An already-terminal invoice (e.g. opening an
      // old SAVED invoice from the list) does one fetch and no interval
      // (Part E). The poll halts itself once it observes a terminal NAV
      // ack + settled email (Part C), or after the 5-minute cap.
      poller.stop();
      if (!isPollTerminal(detailSnapshot(detail))) {
        poller.start();
      }
    } catch (err: unknown) {
      loadState = "error";
      errorMessage = err instanceof Error ? err.message : String(err);
      // Session 158 — a failed (re)load means no live detail to watch;
      // make sure no prior interval keeps firing against a dead fetch.
      poller.stop();
    }
  }

  // PR-44ε.UI / session-58 — trigger the browser-native download
  // dialog. Sequence:
  //   1. Set `downloadState = "downloading"` so the button renders
  //      as disabled with an inline spinner glyph.
  //   2. Invoke `downloadInvoicePdf(invoice_id)`; the Tauri command
  //      forwards to `GET /invoices/<id>/pdf` and returns the raw
  //      PDF bytes wrapped as a `Blob`.
  //   3. Build a synthetic `<a download>` from the Blob URL and
  //      click it to surface the platform-native save dialog. The
  //      object URL is revoked after a short timeout so memory
  //      doesn't leak (the click is synchronous; the browser
  //      finishes the save before the URL goes stale).
  //   4. On failure, surface the error string inline below the
  //      button per CLAUDE.md rule 13 (no toast component).
  //
  // Filename composition: the SPA composes the operator-meaningful
  // shape from `fiscal_year` + `sequence_number`
  // (`invoice_2026-000013.pdf`); see `filenameForInvoice` in
  // `format.ts`. The backend emits its own filename on the
  // `Content-Disposition` header (`pdf_filename_for_invoice` in
  // `serve.rs`) but the Tauri `invoke` bridge does not expose
  // response headers, so the SPA-side composition is authoritative
  // for the browser-saved name.
  async function triggerDownload() {
    if (!detail) return;
    downloadState = "downloading";
    downloadError = null;
    try {
      const blob = await downloadInvoicePdf(detail.invoice_id);
      const composedNumber = `${detail.fiscal_year}-${String(
        detail.sequence_number,
      ).padStart(6, "0")}`;
      const filename = filenameForInvoice(composedNumber);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = filename;
      // Append + click + remove pattern works across every browser
      // engine the Tauri shell ships against (WebKit on macOS, WebView2
      // on Windows). Not appending it would silently no-op on some.
      document.body.appendChild(anchor);
      anchor.click();
      document.body.removeChild(anchor);
      // Revoke after a short delay so the browser's save dialog has
      // time to consume the URL. 1s is generous; the browser
      // typically consumes the URL synchronously on `.click()` and
      // re-resolves on user-confirm, so the revoke is for cleanup
      // not for correctness.
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      downloadState = "idle";
    } catch (err: unknown) {
      downloadState = "error";
      downloadError = err instanceof Error ? err.message : String(err);
    }
  }

  // PR-92 / ADR-0047 — operator-clicked manual "Email to buyer" button.
  // Independent of the post-issue auto-send path (which fires
  // server-side when the operator left the toggle on at issuance).
  // Both paths share the same audit-ledger event kind; the audit
  // payload's `auto: bool` field discriminates.
  async function triggerEmailToBuyer() {
    if (!detail) return;
    mutationState = { kind: "emailing" };
    lastEmailOutcome = null;
    try {
      const outcome = await emailInvoiceToBuyer(detail.invoice_id);
      lastEmailOutcome = outcome;
      // Refetch so the audit-entries table picks up the new
      // InvoiceEmailedSent row.
      await load(detail.invoice_id);
      mutationState = { kind: "idle" };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      mutationState = {
        kind: "error",
        action: "email",
        message,
        navFault: null,
      };
    }
  }

  // PR-44η / session-60 — "Submit to NAV" button handler. POSTs to
  // the backend's `/invoices/<id>/submit` route via the matching
  // Tauri command; on success refetches the detail so the state chip
  // + audit-entries table reflect the new `Submitted` lifecycle
  // state. On failure (including the typed 409 precondition body)
  // the error message lands inline below the header per A157.
  async function triggerSubmit() {
    if (!detail) return;
    mutationState = { kind: "submitting" };
    try {
      await submitInvoice(detail.invoice_id);
      // PR-99 Item 3 — auto-poll once the submit returns. Pre-PR-99 the
      // operator had to click the pictogram (or the legacy Lekérés
      // button before PR-95) AFTER the submit to advance the state from
      // Submitted → Finalized. Without that click, NAV-side SAVED never
      // propagated into the SPA's audit ledger, so the pictogram stayed
      // ⌛, the Latest ack chip stayed `—`, and the action bar never
      // grew the Storno button (which is Finalized-gated). The poll is
      // best-effort: a poll failure (network blip, 31s timeout while
      // NAV still on PROCESSING) leaves the invoice in Submitted with
      // the pictogram clickable so the operator can re-poll later.
      mutationState = { kind: "polling" };
      try {
        await pollAck(detail.invoice_id);
      } catch (_pollErr) {
        // Tolerated: the audit ledger still has the submit entries; the
        // pictogram remains actionable for a manual retry.
      }
      await load(detail.invoice_id);
      mutationState = { kind: "idle" };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      mutationState = {
        kind: "error",
        action: "submit",
        message,
        navFault: parseNavUpstreamFault(message),
      };
    }
  }

  // PR-44η / session-60 — "Poll ack now" button handler. POSTs to
  // the backend's `/invoices/<id>/poll-ack` route; on success
  // refetches the detail so the state chip flips to `Finalized` /
  // `Rejected` (or stays at `Submitted` with the new ack-status
  // chip populated). The bounded poll loop can take up to 31s per
  // ADR-0009 §5; the spinner stays visible the whole time.
  async function triggerPollAck() {
    if (!detail) return;
    mutationState = { kind: "polling" };
    try {
      await pollAck(detail.invoice_id);
      await load(detail.invoice_id);
      mutationState = { kind: "idle" };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      mutationState = {
        kind: "error",
        action: "poll",
        message,
        navFault: parseNavUpstreamFault(message),
      };
    }
  }

  // PR-47β / session-65 — "Amend invoice (modification)" button
  // handler. Unlike storno/submit/poll, modification opens a fresh
  // form modal (operator-edited body); the actual issuance flow
  // lives in `ModificationInvoice.svelte` and the parent listens for
  // its `onAmended` callback to navigate to the new modification
  // invoice. This handler just bubbles up the base's `invoice_id` +
  // `currency` + a displayable identifier to the parent's modal-
  // opening callback.
  function triggerModification() {
    if (!detail) return;
    const baseNumber = `${detail.fiscal_year}-${String(
      detail.sequence_number,
    ).padStart(6, "0")}`;
    // PR-80 / session-102 — pass the base's bank-account snapshot
    // through so the modification form can surface the inherited
    // bank readout. Modification chain children inherit the bank
    // from the base (ADR-0040 §addendum); the form's readout lets
    // the operator confirm the routing before submitting.
    onAmend(detail.invoice_id, detail.currency, baseNumber, detail.bank_account);
  }

  // PR-47α / session-64 — "Cancel invoice (storno)" handlers. Two-
  // stage flow per PR-80 / session-102:
  //
  //   1. `triggerOpenStornoConfirm` — opens the inline confirm panel
  //      below the action bar. The panel shows the invoice's
  //      identifying fields + an explanation of what storno does +
  //      Confirm / Cancel buttons. No NAV call yet.
  //
  //   2. `triggerConfirmStorno` — fires the actual POST to
  //      `/api/invoices/<id>/storno` via the matching Tauri command.
  //      On success refetches the base's detail so the chip flips to
  //      `Storno`, the audit-trail picks up the `InvoiceStornoIssued`
  //      chain-link row, and the `chain_children` list grows to
  //      include the new storno's id. On failure the typed NAV-upstream
  //      fault renders inline like the other mutation routes.
  //
  // The pre-PR-80 posture wrapped both stages behind a single
  // `window.confirm()` which visually disconnected the confirmation
  // from the invoice being cancelled — a stray click on the modal-
  // over-modal dialog was the only thing between the operator and an
  // irreversible audit-ledger write. The inline panel keeps the
  // invoice's identifying fields visible alongside the explanatory
  // copy so the operator confirms with the regulatory record on
  // screen, not a tiny browser dialog.
  function triggerOpenStornoConfirm() {
    if (!detail) return;
    // PR-83 — reset the reason field on every fresh open so a stale
    // value from a prior (cancelled) confirm does not leak.
    stornoReason = "";
    stornoConfirmOpen = true;
  }

  function triggerCancelStornoConfirm() {
    stornoConfirmOpen = false;
    stornoReason = "";
  }

  async function triggerConfirmStorno() {
    if (!detail) return;
    // PR-83 — trim + normalise empty-after-trim to `null` so the wire
    // body matches PR-82's blankToNull rule. The backend's route also
    // normalises (defence in depth); doing it here keeps the wire
    // shape honest for the operator's first-look network inspection.
    const trimmed = stornoReason.trim();
    const reason: string | null = trimmed === "" ? null : trimmed;
    stornoConfirmOpen = false;
    mutationState = { kind: "cancelling" };
    try {
      await cancelInvoiceStorno(detail.invoice_id, { stornoReason: reason });
      // Refetch so the base's audit-trail picks up the
      // InvoiceStornoIssued chain-link row AND the chip flips from
      // `Finalized` → `Storno` (the `is_storno_base` arm of
      // derive_state's priority ladder, ADR-0036 §3) AND the
      // `chain_children` list grows.
      await load(detail.invoice_id);
      stornoReason = "";
      mutationState = { kind: "idle" };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      mutationState = {
        kind: "error",
        action: "cancel",
        message,
        navFault: parseNavUpstreamFault(message),
      };
    }
  }

  // PR-70 / ADR-0039 — open the mark-as-paid dialog with sensible
  // defaults pre-populated from the loaded invoice. `paid_at` is
  // today (the operator can override for a back-dated record);
  // `amount_minor` is the invoice's total_gross (the v1 single-
  // payment-per-invoice posture per ADR-0039 §3); `method` defaults
  // to BankTransfer (Ervin's most-common in the session-81 source).
  function triggerOpenMarkPaid() {
    if (!detail) return;
    const today = new Date();
    const yyyy = today.getFullYear();
    const mm = String(today.getMonth() + 1).padStart(2, "0");
    const dd = String(today.getDate()).padStart(2, "0");
    payForm = {
      paid_at: `${yyyy}-${mm}-${dd}`,
      amount_minor:
        detail.total_gross !== null ? String(detail.total_gross) : "",
      method: "BankTransfer",
      reference: "",
    };
    payFormError = null;
    paymentDialogOpen = true;
    // showModal() is called by the $effect bound to the dialog
    // element below; we just flip the flag here.
  }

  function triggerClosePaymentDialog() {
    paymentDialogOpen = false;
    payFormError = null;
  }

  // PR-70 / ADR-0039 — submit the mark-as-paid form. Validates the
  // amount field locally (must parse to a positive integer) and
  // POSTs the rest verbatim; the backend handles the date-format
  // check (loud 400) and the no-double-pay + state-gate (409).
  async function triggerSubmitMarkPaid(e: SubmitEvent) {
    e.preventDefault();
    if (!detail) return;
    payFormError = null;
    const amount = Number.parseInt(payForm.amount_minor, 10);
    if (!Number.isFinite(amount) || amount <= 0) {
      payFormError =
        "Amount must be a positive integer in the invoice's minor-unit form (whole HUF or EUR cents).";
      return;
    }
    const body: MarkPaidRequest = {
      paid_at: payForm.paid_at,
      amount_minor: amount,
      currency: detail.currency,
      method: payForm.method,
      reference: payForm.reference.trim() === "" ? null : payForm.reference.trim(),
    };
    mutationState = { kind: "paying" };
    try {
      await markInvoicePaid(detail.invoice_id, body);
      // Refetch so the Paid chip + payment meta-grid pick up the
      // new audit entry. The fetched detail is the same shape
      // `getInvoice` already returns; no special handling needed.
      await load(detail.invoice_id);
      mutationState = { kind: "idle" };
      paymentDialogOpen = false;
      payFormError = null;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      // The 409 already-paid branch carries a typed body; extract
      // it and render the existing payment record inline rather
      // than the generic "internal error" string. Same posture as
      // `parseNavUpstreamFault`.
      const alreadyPaid = parseAlreadyPaidError(message);
      if (alreadyPaid !== null) {
        payFormError = `Already paid on ${alreadyPaid.payment.paid_at} (${alreadyPaid.payment.method}). Refresh to see the recorded payment.`;
      } else {
        payFormError = message;
      }
      mutationState = {
        kind: "error",
        action: "pay",
        message,
        navFault: null,
      };
    }
  }

  // Drive the payment-dialog open/close lifecycle from the flag.
  $effect(() => {
    if (!paymentDialogEl) return;
    if (paymentDialogOpen) {
      if (!paymentDialogEl.open) paymentDialogEl.showModal();
    } else {
      if (paymentDialogEl.open) paymentDialogEl.close();
    }
  });

  function signalClass(signal: LabelSignal): string {
    return `signal-${signal}`;
  }

  // PR-80 / session-102 — dispatch the matching handler for the
  // operator-clicked action button. Routes through `detailActionMeta`'s
  // closed-vocab so a new variant requires a paired arm here (the
  // TypeScript exhaustiveness check on the switch surfaces a missing
  // arm at `npm run check`).
  function dispatchAction(button: DetailActionButton): void {
    switch (button) {
      case "Submit":
        void triggerSubmit();
        return;
      case "Pay":
        triggerOpenMarkPaid();
        return;
      case "Storno":
        triggerOpenStornoConfirm();
        return;
      case "Modification":
        triggerModification();
        return;
      case "Download":
        void triggerDownload();
        return;
      case "Email":
        void triggerEmailToBuyer();
        return;
    }
  }

  // PR-80 / session-102 — Hungarian busy-state label per button. A
  // regression that collapsed every busy state to "…" would erase the
  // affordance during the most ambiguous part of the flow (the
  // operator clicked → something is happening → what?). Keeping the
  // labels visible during the spinner reads as a professional product.
  function actionBusyLabel(button: DetailActionButton): string {
    switch (button) {
      case "Submit":
        return "Beküldés folyamatban…";
      case "Pay":
        return "Fizetés rögzítése…";
      case "Storno":
        return "Sztornózás folyamatban…";
      case "Modification":
        return "Módosítás…";
      case "Download":
        return "Letöltés…";
      case "Email":
        return "Email küldése…";
    }
  }

  // ESC + backdrop dismiss both fire the native `close` event; we
  // mirror it back to the parent so the parent's `selectedId` resets.
  // Without this, a second click on the same row would not re-open
  // because `invoiceId` never transitioned through `null`.
  function handleDialogClose() {
    onClose();
  }

  // Clicking the dialog backdrop closes the dialog. The native
  // <dialog> only treats clicks on the dialog element itself (not
  // its children) as backdrop clicks; we forward those to `close()`.
  function handleDialogClick(e: MouseEvent) {
    if (e.target === dialogEl) {
      dialogEl?.close();
    }
  }

  // PR-44ε / session-53 — currency-aware total formatter lives in
  // `../lib/format`. The pre-PR-44ε inline `hufFormatter` +
  // `formatHuf` pair is removed; the same module also serves the
  // four new rate-metadata rows (`formatRate`, `formatRateDate`,
  // `formatHufEquivalent`). See InvoiceList.svelte for the
  // matching deletion.

  // PR-27 — toggle the expansion state of a single audit row.
  // Reassignment pattern (see the `expandedSeqs` declaration
  // comment for rationale).
  function toggleExpand(seq: number) {
    const next = new Set(expandedSeqs);
    if (next.has(seq)) {
      next.delete(seq);
    } else {
      next.add(seq);
    }
    expandedSeqs = next;
  }

  // PR-27 — pretty-print the typed payload for the drill-down sub-
  // row. PR-29 / session-33 added the `bytesAsUtf8Replacer` from
  // `../lib/payload-reviver` so the `Vec<u8>` fields
  // `audit_payloads::*` carries (request_xml / response_xml /
  // ack_xml / failure response_xml / annulment request_xml) render
  // as decoded XML instead of long JSON int arrays. Non-UTF-8 byte
  // arrays (rare; would indicate a non-XML body in a `Vec<u8>`
  // field — NAV always emits UTF-8 XML per v3.0 spec) pass through
  // as the raw int array so no information is lost. See
  // `payload-reviver.ts` for the heuristic and the future-drift
  // note about hypothetical `Vec<integer>` payload fields.
  function formatPayload(payload: unknown): string {
    try {
      return JSON.stringify(payload, bytesAsUtf8Replacer, 2);
    } catch {
      // Should not happen for serde_json::Value — the field is
      // already JSON-typed. Defence-in-depth so a future malformed
      // shape (e.g., circular reference if the renderer is reused
      // somewhere unexpected) does not crash the modal.
      return String(payload);
    }
  }
</script>

<dialog
  bind:this={dialogEl}
  class="detail"
  onclose={handleDialogClose}
  onclick={handleDialogClick}
  aria-label="Invoice detail"
>
  <div class="detail-frame">
    <header class="detail-head">
      <div class="detail-title">
        {#if ancestors.length > 0}
          <nav class="breadcrumb" aria-label="Navigation history">
            {#each ancestors as ancestorId, i (i)}
              <button
                type="button"
                class="breadcrumb-step mono"
                onclick={() => onJumpBack(i)}
                aria-label={`Back to invoice ${ancestorId}`}
                title={`Back to invoice ${ancestorId}`}
              >
                ← {ancestorId}
              </button>
            {/each}
          </nav>
        {/if}
        <span class="detail-label">Invoice</span>
        <div class="detail-id-row">
          <h2 class="detail-id mono">{invoiceId ?? ""}</h2>
          {#if detail !== null}
            <!-- PR-95 / session-115 — NAV-status pictogram in the
                 detail header. Prominent placement next to the invoice
                 id so the operator's eye lands on the 4-state ack
                 signal first. Click-to-recheck on `InFlight` re-polls
                 NAV (same `pollAck` Tauri call the obsoleted action-bar
                 PollAck button hit). Terminal states render as plain
                 spans with the tooltip-on-hover convention. -->
            {@const pictogram = navStatusPictogram(detail.state)}
            {@const pictogramBusy = mutationState.kind === "polling"}
            {#if pictogram.actionable}
              <button
                type="button"
                class="nav-pictogram-detail {pictogram.kind_class} actionable"
                class:busy={pictogramBusy}
                onclick={() => void triggerPollAck()}
                disabled={mutationState.kind !== "idle"}
                aria-label={pictogram.tooltip_en}
                title={`${pictogram.tooltip_hu} / ${pictogram.tooltip_en}`}
                data-testid="nav-pictogram"
              >
                <span aria-hidden="true">
                  {pictogramBusy ? "…" : pictogram.glyph}
                </span>
              </button>
            {:else}
              <span
                class="nav-pictogram-detail {pictogram.kind_class}"
                aria-label={pictogram.tooltip_en}
                title={`${pictogram.tooltip_hu} / ${pictogram.tooltip_en}`}
                data-testid="nav-pictogram"
              >
                <span aria-hidden="true">{pictogram.glyph}</span>
              </span>
            {/if}
          {/if}
        </div>
      </div>
      <div class="detail-actions">
        <button
          type="button"
          class="quiet-button"
          onclick={() => dialogEl?.close()}
          aria-label="Close invoice detail"
          title="Bezárás"
        >
          Bezár
        </button>
      </div>
    </header>

    <!-- PR-80 / session-102 — operator action bar. The pre-PR-80 posture
         packed every button into the modal header's `.detail-actions`
         row with no visual hierarchy; the operator scanned an
         ambiguous strip of buttons with no signal about which actions
         move through the NAV ladder versus which spawn chain children
         versus which export an artifact. PR-80 elevates this to a
         dedicated action bar sectioned by `groupButtons`: Lifecycle
         (NAV submit/poll) → Operational (Pay) → Chain (Storno /
         Modification) → Export (Download). Each section carries a
         bilingual label; buttons within a section render with a
         consistent glyph + Hungarian label + tooltip vocabulary
         sourced from `detailActionMeta` so the affordance is identical
         to the row-level quick-action surface. -->
    {#if detail}
      {@const buttons = buttonsForState(detail.state, detail.payment !== null)}
      {@const groups = groupButtons(buttons)}
      {@const mutationBusy =
        mutationState.kind === "submitting" ||
        mutationState.kind === "polling" ||
        mutationState.kind === "cancelling" ||
        mutationState.kind === "paying" ||
        mutationState.kind === "emailing"}
      {#if groups.length > 0}
        <div class="action-bar" role="toolbar" aria-label="Számla műveletek">
          {#each groups as group (group.group)}
            {@const groupLabel = actionGroupLabel(group.group)}
            <div
              class="action-group"
              data-group={group.group}
              data-testid={`action-group-${group.group}`}
            >
              <p class="action-group-label" title={groupLabel.label_en}>
                {groupLabel.label_hu}
              </p>
              <div class="action-group-buttons">
                {#if group.group === "Export" && lastSuccessfulEmail !== null}
                  <!-- PR-99 Item 4 Part A — once an `InvoiceEmailedSent`
                       succeeded audit row exists for this invoice, show
                       the sent-status pill so the operator sees the
                       record at a glance. The Email button below is
                       swapped to a Re-send affordance so re-sends are
                       still possible but the first-time-send affordance
                       does not visually hide the fact that NAV-side
                       state has already moved on. -->
                  <p class="email-sent-status" data-testid="email-sent-status">
                    <span class="action-glyph" aria-hidden="true">✉</span>
                    <span>
                      Elküldve {formatEmailSentTime(lastSuccessfulEmail.occurredAt)}
                      · Sent {formatEmailSentTime(lastSuccessfulEmail.occurredAt)}
                    </span>
                    <code class="email-sent-recipient">
                      {lastSuccessfulEmail.recipient}
                    </code>
                  </p>
                {/if}
                {#each group.buttons as button (button)}
                  {@const meta = detailActionMeta(button)}
                  {@const busy =
                    (button === "Submit" && mutationState.kind === "submitting") ||
                    (button === "Pay" && mutationState.kind === "paying") ||
                    (button === "Storno" && mutationState.kind === "cancelling") ||
                    (button === "Email" && mutationState.kind === "emailing") ||
                    (button === "Download" && downloadState === "downloading")}
                  {@const disabled =
                    (button === "Download"
                      ? downloadState === "downloading" || mutationBusy
                      : mutationBusy) ||
                    (button === "Storno" && stornoConfirmOpen)}
                  {@const resend =
                    button === "Email" && lastSuccessfulEmail !== null}
                  {@const labelHu = resend ? "Újraküldés" : meta.label_hu}
                  {@const labelEn = resend ? "Re-send" : meta.label_en}
                  {@const glyph = resend ? "↻" : meta.glyph}
                  {@const tooltip = resend
                    ? "Újabb e-mail küldése a vevőnek (új audit-bejegyzéssel)."
                    : meta.tooltip_hu}
                  <button
                    type="button"
                    class="action-button {busy ? 'action-button-busy' : ''}"
                    onclick={() => dispatchAction(button)}
                    disabled={disabled}
                    aria-label={labelEn}
                    title={tooltip}
                    data-testid={`action-button-${button}`}
                  >
                    {#if busy}
                      <span class="action-glyph" aria-hidden="true">…</span>
                      <span class="action-label">
                        {actionBusyLabel(button)}
                      </span>
                    {:else}
                      <span class="action-glyph" aria-hidden="true">{glyph}</span>
                      <span class="action-label">{labelHu}</span>
                    {/if}
                  </button>
                {/each}
              </div>
            </div>
          {/each}
        </div>
      {/if}

      {#if lastEmailOutcome !== null}
        <!-- PR-92 / ADR-0047 — inline email-send outcome banner. Mirror
             of the audit-ledger entry the backend just wrote; visible
             to the operator immediately so they don't have to scan the
             audit log table for confirmation. -->
        <div
          class="email-outcome email-outcome--{lastEmailOutcome.outcome}"
          role={lastEmailOutcome.outcome === "failed" ? "alert" : "status"}
          data-testid="invoice-email-outcome"
        >
          {#if lastEmailOutcome.outcome === "succeeded"}
            <strong>✉ Email elküldve · Email sent</strong>
            <p class="email-outcome__detail">
              Címzett · Recipient: <code>{lastEmailOutcome.recipient}</code>
              {#if lastEmailOutcome.attached_xml}
                · NAV XML csatolva · XML attached
              {/if}
            </p>
          {:else}
            <strong>✉ Email küldés sikertelen · Email failed</strong>
            <p class="email-outcome__detail">
              Hibaosztály · Class:
              <code>{lastEmailOutcome.error_class ?? "other"}</code>
              · Címzett · Recipient: <code>{lastEmailOutcome.recipient}</code>
            </p>
            {#if lastEmailOutcome.error_detail}
              <p class="email-outcome__detail">
                {lastEmailOutcome.error_detail}
              </p>
            {/if}
          {/if}
        </div>
      {/if}

      <!-- PR-80 / session-102 — inline storno confirm panel. Replaces
           the pre-PR-80 `window.confirm()` modal-over-modal that
           visually disconnected the confirmation from the invoice
           being cancelled. The panel surfaces the invoice's
           identifying fields alongside the explanatory copy so the
           operator confirms with the regulatory record on screen. -->
      {#if stornoConfirmOpen}
        <div
          class="storno-confirm"
          role="alertdialog"
          aria-labelledby="storno-confirm-title"
          data-testid="storno-confirm-panel"
        >
          <p id="storno-confirm-title" class="storno-confirm-title">
            <span aria-hidden="true">⊘</span>
            Sztornó megerősítése
          </p>
          <dl class="storno-confirm-grid">
            <dt>Számlaszám</dt>
            <dd class="mono">
              {detail.fiscal_year}-{String(detail.sequence_number).padStart(
                6,
                "0",
              )}
            </dd>
            <dt>Pénznem</dt>
            <dd class="mono">{detail.currency}</dd>
            <dt>Bruttó végösszeg</dt>
            <dd class="mono">
              {formatTotal(detail.total_gross, detail.currency)}
            </dd>
          </dl>
          <p class="storno-confirm-copy">
            Egy ellentétes előjelű sztornó számla kerül kiállításra
            ugyanezen a pénznemen és árfolyamon. Új sorszámot foglal le
            (ADR-0023 §3), és három audit-bejegyzést ír; ez a művelet
            <strong>nem visszafordítható</strong>. A beküldéshez ezután
            külön NAV beküldés szükséges.
          </p>
          <!-- PR-83 / session-103 — buyer-facing storno reason. Free-
               text "here's why we cancelled" note that lands on the
               storno's printed PDF / future email body so the buyer
               sees it on the same document. Optional; blank leaves
               the storno's `invoice_note` column NULL. NEVER reaches
               the NAV XML — see ADR-0042. Bilingual label + hint
               make the recipient-facing surface obvious so the
               operator does not type internal-only notes here. -->
          <label class="storno-reason-label" for="storno-reason-input">
            <span class="storno-reason-label-text">
              Sztornó indoka / Storno reason
            </span>
            <span class="storno-reason-hint">
              Megjelenik a vevő példányán / Shown on the buyer's copy
            </span>
          </label>
          <textarea
            id="storno-reason-input"
            class="storno-reason-input"
            bind:value={stornoReason}
            maxlength="4000"
            rows="3"
            placeholder="pl. Téves vevő adatok kerültek a számlára…"
            data-testid="storno-confirm-reason"
          ></textarea>
          <div class="storno-confirm-actions">
            <button
              type="button"
              class="action-button"
              onclick={triggerCancelStornoConfirm}
              data-testid="storno-confirm-cancel"
            >
              <span class="action-label">Mégse</span>
            </button>
            <button
              type="button"
              class="action-button action-button-danger"
              onclick={triggerConfirmStorno}
              data-testid="storno-confirm-accept"
            >
              <span class="action-glyph" aria-hidden="true">⊘</span>
              <span class="action-label">Igen, sztornózás</span>
            </button>
          </div>
        </div>
      {/if}
    {/if}
    {#if downloadState === "error" && downloadError}
      <p class="error download-error" role="alert">
        Download failed: {downloadError}
      </p>
    {/if}
    {#if mutationState.kind === "error"}
      {#if mutationState.navFault}
        <!-- PR-58 / session-78 — typed NAV upstream-fault rendering.
             The backend returned HTTP 502 with the parsed fault_code +
             Hungarian-localized fault_message; render both prominently
             so the operator can act (e.g. IP whitelist mismatch,
             expired technical-user password, signature drift).
             PR-59 / session-79 — also render the per-rule
             technical_validations list NAV emits inside
             <technicalValidationMessages>. For NAV's most common 400
             (fault_code=INVALID_REQUEST) the top-level wrapper is
             generic; the actual reject reason is in this list. The
             raw body preview is the fallback evidence for the cases
             NAV returns a shape the backend parser does not recognise. -->
        <div class="error download-error nav-fault" role="alert">
          <p class="nav-fault-headline">
            {mutationState.action === "submit"
              ? "Submit"
              : mutationState.action === "poll"
                ? "Poll ack"
                : "Cancel invoice"} rejected by NAV (HTTP {mutationState.navFault.status})
          </p>
          <dl class="nav-fault-grid">
            <dt>Fault code</dt>
            <dd class="mono">
              {mutationState.navFault.fault_code ?? "<no fault code>"}
            </dd>
            <dt>Fault message</dt>
            <dd>
              {mutationState.navFault.fault_message ?? "<no fault message>"}
            </dd>
          </dl>
          {#if mutationState.navFault.technical_validations.length > 0}
            <p class="nav-fault-validations-heading">
              NAV validation messages ({mutationState.navFault
                .technical_validations.length})
            </p>
            <ul class="nav-fault-validations">
              {#each mutationState.navFault.technical_validations as v, i (i)}
                <li class="nav-fault-validation">
                  <div class="nav-fault-validation-head">
                    <span class="mono nav-fault-validation-code"
                      >{v.error_code ?? "<no code>"}</span
                    >
                    {#if v.result_code}
                      <span
                        class="mono nav-fault-validation-result {v.result_code ===
                        'WARN'
                          ? 'warn'
                          : 'error'}"
                      >
                        {v.result_code}
                      </span>
                    {/if}
                  </div>
                  {#if v.message}
                    <p class="nav-fault-validation-message">{v.message}</p>
                  {/if}
                  {#if v.tag}
                    <p class="mono nav-fault-validation-tag">{v.tag}</p>
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}
          <details class="nav-fault-body-details">
            <summary>Body preview</summary>
            <pre class="nav-fault-body">{mutationState.navFault
                .raw_body_preview}</pre>
          </details>
        </div>
      {:else}
        <p class="error download-error" role="alert">
          {mutationState.action === "submit"
            ? "Submit"
            : mutationState.action === "poll"
              ? "Poll ack"
              : "Cancel invoice"} failed: {mutationState.message}
        </p>
      {/if}
    {/if}

    {#if loadState === "loading"}
      <p class="muted">Loading…</p>
    {:else if loadState === "error"}
      <p class="error" role="alert">{errorMessage}</p>
    {:else if loadState === "loaded" && detail}
      {@const meta = labelMeta(detail.state)}
      <dl class="meta-grid">
        <dt>Series #</dt>
        <dd class="mono">{detail.sequence_number}</dd>
        <dt>Fiscal year</dt>
        <dd class="mono">{detail.fiscal_year}</dd>
        <!-- PR-99 Item 5 — the three operator-meaningful invoice
             dates. PR-84 added the picker on the IssueInvoice form;
             this PR surfaces all three on the detail meta-grid so the
             operator can verify what was actually committed (the
             server stamps the immutable issue date from its own
             clock; payment_deadline + delivery_date land from the
             form). Bilingual labels per ADR-0036's HU + EN
             affordance precedent; date format matches the printed
             PDF (YYYY. MM. DD.) so cross-referencing the document is
             eye-friction-free. -->
        <dt>Számla kelte / Issue date</dt>
        <dd class="mono" data-testid="detail-issue-date">
          {formatInvoiceDate(detail.issue_date)}
        </dd>
        <dt>Fizetési határidő / Payment deadline</dt>
        <dd class="mono" data-testid="detail-payment-deadline">
          {formatInvoiceDate(detail.payment_deadline)}
        </dd>
        <dt>Teljesítési dátum / Delivery date</dt>
        <dd class="mono" data-testid="detail-delivery-date">
          {formatInvoiceDate(detail.delivery_date)}
        </dd>
        <dt>State</dt>
        <dd>
          <span
            class="state-pill {signalClass(meta.signal)}"
            title={meta.tooltip}
          >
            <span class="state-icon" aria-hidden="true">{meta.icon}</span>
            <span class="state-text">{detail.state}</span>
          </span>
          {#if detail.payment !== null}
            <!-- PR-70 / ADR-0039 §2 — operational Paid badge. Sits
                 next to the regulatory state chip; paid-vs-unpaid is
                 parallel operational metadata, NOT a NAV-ladder
                 transition (the state chip continues to read
                 `Finalized`, the SAVED-ack terminal). -->
            <span class="state-pill paid-pill" title={`Paid on ${detail.payment.paid_at}`}>
              <span class="state-icon" aria-hidden="true">✓</span>
              <span class="state-text">Paid</span>
            </span>
          {/if}
        </dd>
        <dt>Total (gross)</dt>
        <dd class="mono">
          {formatInvoiceTotal(
            detail.total_gross,
            detail.currency,
            detail.is_storno,
          )}
        </dd>
        {#if detail.currency !== "HUF" && detail.exchange_rate !== null}
          <dt>Exchange rate</dt>
          <dd class="mono">{formatRate(detail.exchange_rate)}</dd>
        {/if}
        {#if detail.currency !== "HUF" && detail.exchange_rate_source !== null}
          <dt>Rate source</dt>
          <dd class="mono">{detail.exchange_rate_source}</dd>
        {/if}
        {#if detail.currency !== "HUF" && detail.exchange_rate_date !== null}
          <dt>Rate date</dt>
          <dd class="mono">{formatRateDate(detail.exchange_rate_date)}</dd>
        {/if}
        {#if detail.currency !== "HUF" && detail.huf_equivalent_total !== null}
          <dt>HUF equivalent</dt>
          <dd class="mono">
            {formatHufEquivalent(
              detail.is_storno
                ? -detail.huf_equivalent_total
                : detail.huf_equivalent_total,
            )}
          </dd>
        {/if}
        <dt>Latest ack</dt>
        <dd>
          {#if detail.last_ack_status === null}
            <span class="mono">—</span>
          {:else}
            {@const ackMeta = ackLabelMeta(detail.last_ack_status)}
            <span
              class="state-pill {signalClass(ackMeta.signal)}"
              title={ackMeta.tooltip}
            >
              <span class="state-icon" aria-hidden="true">{ackMeta.icon}</span>
              <span class="state-text">{detail.last_ack_status}</span>
            </span>
          {/if}
        </dd>
        {#if detail.payment !== null}
          <!-- PR-70 / ADR-0039 §2 — payment details rendered after
               the regulatory ladder rows so the operator's eye lands
               on the NAV state first, then the operational payment
               metadata. -->
          <dt>Paid on</dt>
          <dd class="mono">{detail.payment.paid_at}</dd>
          <dt>Paid amount</dt>
          <dd class="mono">{formatTotal(detail.payment.amount_minor, detail.currency)}</dd>
          <dt>Payment method</dt>
          <dd class="mono">{detail.payment.method}</dd>
          {#if detail.payment.reference !== null}
            <dt>Payment reference</dt>
            <dd class="mono">{detail.payment.reference}</dd>
          {/if}
        {/if}
        <!-- PR-73 / ADR-0040 §addendum — operator-facing "Pay to"
             sub-section. Renders the per-invoice bank-account
             snapshot the invoice was issued with; falls back to a
             muted em-dash placeholder on `null` (pre-PR-73 /
             CLI-issued invoices). -->
        <dt>Pay to</dt>
        <dd data-testid="invoice-detail-bank-account">
          {#if detail.bank_account === null}
            <span class="mono">—</span>
          {:else}
            <div class="bank-snapshot">
              <span class="mono">{detail.bank_account.bank_name}</span>
              <span class="mono">{detail.bank_account.account_number}</span>
              <span class="mono">SWIFT/BIC: {detail.bank_account.swift_bic}</span>
            </div>
          {/if}
        </dd>
      </dl>

      <!-- PR-82 — buyer-facing notes ("Megjegyzés") section. Renders
           when the operator typed an invoice-level note OR at least
           one line carries a per-line note. Both come from the
           detail wire shape; the printed PDF shows the same content.
           NEVER on the NAV XML wire — see ADR-0042. -->
      {#if detail.invoice_note !== null || detail.line_notes.length > 0}
        <h3 class="section-head">Megjegyzés / Note</h3>
        {#if detail.invoice_note !== null}
          <p class="invoice-note-text" data-testid="detail-invoice-note">
            {detail.invoice_note}
          </p>
        {/if}
        {#if detail.line_notes.length > 0}
          <ul class="line-notes-list" data-testid="detail-line-notes">
            {#each detail.line_notes as ln (ln.ordinal)}
              <li>
                <span class="line-note-prefix">
                  Line {ln.ordinal + 1} ({ln.description}):
                </span>
                <span class="line-note-text">{ln.note}</span>
              </li>
            {/each}
          </ul>
        {/if}
      {/if}

      {#if detail.chain_children.length > 0}
        <h3 class="section-head">Chain children</h3>
        <ul class="chain-children-list">
          {#each detail.chain_children as child (child.invoice_id)}
            {@const childMeta = labelMeta(child.kind)}
            <li class="mono">
              <span class="chain-index-prefix">#{child.modification_index}</span>
              <span
                class="state-pill {signalClass(childMeta.signal)}"
                title={childMeta.tooltip}
              >
                <span class="state-icon" aria-hidden="true">{childMeta.icon}</span>
                <span class="state-text">{child.kind}</span>
              </span>
              <span class="chain-arrow" aria-hidden="true">→</span>
              <button
                type="button"
                class="id-link"
                onclick={() => onNavigate(child.invoice_id)}
                aria-label={`Navigate to chain child invoice ${child.invoice_id} (modification index ${child.modification_index})`}
              >
                {child.invoice_id}
              </button>
            </li>
          {/each}
        </ul>
      {/if}

      <h3 class="section-head">Audit trail</h3>
      <!-- PR-67 / session-89 — visual lifecycle timeline. Replaces
           the pre-PR-67 dense audit-row table as the default view.
           The pure-module helper in `../lib/invoice-timeline.ts`
           does every kind-dispatch decision (glyph, kind_class,
           label, body lines) so the Svelte component stays
           presentational. The raw table sits beneath a "Show raw
           table" toggle for power-user inspection (payload drill-
           down + chain-link navigation affordance). -->
      <InvoiceTimeline nodes={timelineFromAuditEntries(detail.audit_entries)} />
      {#if detail.audit_entries.length > 0}
        <button
          type="button"
          class="raw-table-toggle"
          onclick={() => (showRawTable = !showRawTable)}
          aria-expanded={showRawTable}
        >
          {showRawTable ? "Hide raw table" : "Show raw table"}
        </button>
        {#if showRawTable}
          <table class="dense">
            <thead>
              <tr>
                <th scope="col" class="col-num">Seq</th>
                <th scope="col" class="col-kind">Kind</th>
                <th scope="col" class="col-actor">Actor</th>
                <th scope="col" class="col-time">Occurred at</th>
              </tr>
            </thead>
            <tbody>
              {#each detail.audit_entries as entry (entry.seq)}
                {@const expanded = expandedSeqs.has(entry.seq)}
                <tr>
                  <td class="col-num mono">{entry.seq}</td>
                  <td class="col-kind mono">
                    <button
                      type="button"
                      class="expand-toggle"
                      onclick={() => toggleExpand(entry.seq)}
                      aria-expanded={expanded}
                      aria-label={expanded
                        ? `Hide payload for seq ${entry.seq}`
                        : `Show payload for seq ${entry.seq}`}
                    >
                      {expanded ? "▾" : "▸"}
                    </button>
                    {entry.kind}
                    {#if entry.chain_base_invoice_id}
                      <span class="chain-arrow" aria-hidden="true">→</span>
                      <button
                        type="button"
                        class="id-link"
                        onclick={() => onNavigate(entry.chain_base_invoice_id!)}
                        aria-label={`Navigate to base invoice ${entry.chain_base_invoice_id}`}
                      >
                        {entry.chain_base_invoice_id}
                      </button>
                    {/if}
                  </td>
                  <td class="col-actor mono">{entry.actor}</td>
                  <td class="col-time mono">{entry.occurred_at}</td>
                </tr>
                {#if expanded}
                  <tr class="payload-row">
                    <td colspan="4">
                      <pre class="payload-json">{formatPayload(entry.payload)}</pre>
                    </td>
                  </tr>
                {/if}
              {/each}
            </tbody>
          </table>
        {/if}
      {/if}
    {/if}
  </div>
</dialog>

<!-- PR-70 / ADR-0039 §2 — mark-as-paid form modal. Nested inline
     <dialog> so the form mounts/unmounts cleanly with the parent
     InvoiceDetail modal; the parent's dialog stays open behind it
     (same posture as the Storno confirm via window.confirm — but
     this form needs four fields so a richer UI is justified). -->
<dialog
  bind:this={paymentDialogEl}
  class="detail pay-dialog"
  onclose={triggerClosePaymentDialog}
  aria-label="Mark invoice as paid"
>
  {#if detail !== null}
    <form class="pay-form" onsubmit={triggerSubmitMarkPaid}>
      <h3 class="pay-title">Mark invoice as paid</h3>
      <p class="pay-subtitle mono">
        Invoice {detail.invoice_id}
      </p>
      <label class="pay-row">
        <span class="pay-label">Paid on</span>
        <input
          type="date"
          required
          bind:value={payForm.paid_at}
          disabled={mutationState.kind === "paying"}
        />
      </label>
      <label class="pay-row">
        <span class="pay-label">
          Amount ({detail.currency === "EUR" ? "EUR cents" : "HUF"})
        </span>
        <input
          type="number"
          required
          min="1"
          step="1"
          bind:value={payForm.amount_minor}
          disabled={mutationState.kind === "paying"}
        />
      </label>
      <label class="pay-row">
        <span class="pay-label">Currency</span>
        <input
          type="text"
          readonly
          value={detail.currency}
          class="mono"
        />
      </label>
      <label class="pay-row">
        <span class="pay-label">Payment method</span>
        <select
          bind:value={payForm.method}
          disabled={mutationState.kind === "paying"}
        >
          <option value="BankTransfer">Bank transfer (Átutalás)</option>
          <option value="Cash">Cash (Készpénz)</option>
          <option value="Card">Card (Kártya)</option>
          <option value="Other">Other (Egyéb)</option>
        </select>
      </label>
      <label class="pay-row">
        <span class="pay-label">Reference (optional)</span>
        <input
          type="text"
          placeholder="Bank transaction id, cheque #, …"
          bind:value={payForm.reference}
          disabled={mutationState.kind === "paying"}
        />
      </label>
      {#if payFormError !== null}
        <p class="error pay-error" role="alert">{payFormError}</p>
      {/if}
      <div class="pay-actions">
        <button
          type="button"
          class="quiet-button"
          onclick={triggerClosePaymentDialog}
          disabled={mutationState.kind === "paying"}
        >
          Cancel
        </button>
        <button
          type="submit"
          class="quiet-button"
          disabled={mutationState.kind === "paying"}
        >
          {#if mutationState.kind === "paying"}
            <span aria-hidden="true">…</span> Recording payment
          {:else}
            Record payment
          {/if}
        </button>
      </div>
    </form>
  {/if}
</dialog>

<style>
  /* Native <dialog> reset — the platform default carries chrome
   * (border, padding, background) that fights ADR-0017's quiet
   * surfaces. */
  dialog.detail {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 720px;
    overflow: hidden;
  }

  dialog.detail::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .detail-frame {
    display: flex;
    flex-direction: column;
    max-height: 90vh;
    overflow: auto;
    padding: var(--space-4) var(--space-5);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .detail-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-4);
  }

  .detail-title {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .detail-label {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
  }

  .detail-id-row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-wrap: wrap;
  }

  .detail-id {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
    word-break: break-all;
  }

  /* PR-95 / session-115 — prominent NAV-status pictogram next to the
   * detail header's invoice id. Sized larger than the list-row
   * variant (1.6em there → 2em here) so the operator's eye lands on
   * the ack signal first. The four kind-classes share the same
   * signal-token vocabulary as the list-row pictogram so the visual
   * palette is consistent across surfaces. */
  .nav-pictogram-detail {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 2em;
    height: 2em;
    padding: 0;
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    background: var(--color-surface-base);
    color: var(--color-text-secondary);
    font-family: var(--type-family-body);
    font-size: var(--type-size-md);
    line-height: 1;
    cursor: help;
  }

  .nav-pictogram-detail.pictogram-muted {
    color: var(--color-text-muted);
    border-color: var(--color-surface-divider);
  }
  /* PR-98 — `pictogram-submitted` is the green-toned positive-in-
   * progress kind for the post-submit-pre-terminal `Submitted` /
   * `Recovered` lifecycle pair. */
  .nav-pictogram-detail.pictogram-submitted {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }
  .nav-pictogram-detail.pictogram-negative {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .nav-pictogram-detail.pictogram-positive {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  button.nav-pictogram-detail.actionable {
    cursor: pointer;
  }

  button.nav-pictogram-detail.actionable:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  button.nav-pictogram-detail.actionable:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  button.nav-pictogram-detail.actionable:disabled {
    opacity: 0.7;
    cursor: progress;
  }

  button.nav-pictogram-detail.busy {
    cursor: progress;
  }

  /* PR-30 — breadcrumb / back-button trail. One quiet `← {id}`
   * button per ancestor; clicking jumps the parent's navigation
   * stack back to that level. Same aesthetic as `.id-link` (quiet
   * chrome, underline-on-hover) per ADR-0017 §1-2. Wraps on a
   * narrow modal — each segment stays atomic. */
  .breadcrumb {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-1) var(--space-3);
    margin-bottom: var(--space-1);
  }

  .breadcrumb-step {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    cursor: pointer;
    text-align: left;
    word-break: break-all;
  }

  .breadcrumb-step:hover,
  .breadcrumb-step:focus-visible {
    color: var(--color-text-strong);
    text-decoration: underline;
    text-decoration-color: var(--color-text-muted);
    text-underline-offset: 2px;
  }

  .breadcrumb-step:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    transition: color var(--motion-fade-in);
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quiet-button:disabled {
    cursor: progress;
    opacity: 0.7;
  }

  /* PR-44ε.UI — header actions row. PR-80 lifted the operator action
   * buttons out into a dedicated `.action-bar` (see below); this row
   * now only carries the Close button. */
  .detail-actions {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }

  /* PR-80 / session-102 — operator action bar. Sectioned by
   * `groupButtons` into Lifecycle / Operational / Chain / Export so
   * the operator scans a coherent hierarchy. Each section's header
   * is a quiet uppercase label per the dt/dd label convention
   * (ADR-0017 §1-2). Buttons within a section sit on a raised
   * surface so the action bar reads as a distinct workspace zone
   * within the modal. */
  .action-bar {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
    margin: 0 0 var(--space-4) 0;
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    border-radius: var(--radius-md, 6px);
  }

  .action-group {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    /* Keep groups visually distinct via a vertical hairline rather
     * than another box — too many nested boxes flatten the hierarchy.
     * The hairline lives on the LEFT of every group except the
     * first, drawn via the next-sibling combinator below. */
  }

  .action-group + .action-group {
    padding-left: var(--space-4);
    border-left: 1px solid var(--color-surface-divider);
  }

  .action-group-label {
    margin: 0;
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
    cursor: help;
  }

  .action-group-buttons {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }

  /* PR-99 Item 4 Part A — inline "Elküldve HH:MM" status pill rendered
   * before the Re-send button inside the Export group. Operator-
   * facing surface only; the audit log table carries the durable
   * record. */
  .email-sent-status {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    margin: 0 var(--space-1) 0 0;
    padding: var(--space-1) var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-positive);
    border-radius: var(--radius-md, 6px);
    color: var(--color-signal-positive);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
  }

  .email-sent-recipient {
    font-family: var(--type-family-mono);
    color: var(--color-text-secondary);
  }

  /* PR-80 / session-102 — operator action button. Sized larger than
   * the chrome-quiet `.quiet-button` so the action bar reads as the
   * operator's primary task surface; still uses the dense token
   * vocabulary (no accent colour by default; the danger variant for
   * destructive confirms uses the negative signal token). */
  .action-button {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    transition: color var(--motion-fade-in), border-color var(--motion-fade-in);
    min-height: 2rem;
  }

  .action-button:hover:not(:disabled) {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .action-button:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  .action-button:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }

  .action-button-busy {
    cursor: progress !important;
    opacity: 0.8;
  }

  .action-button-danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .action-button-danger:hover:not(:disabled) {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
    background: var(--color-surface-base);
  }

  .action-glyph {
    font-family: var(--type-family-body);
    font-size: var(--type-size-md);
    line-height: 1;
  }

  .action-label {
    line-height: 1.2;
  }

  /* PR-92 / ADR-0047 — inline email-send outcome banner. Sits below
   * the action bar; positive-signal accent on success, negative on
   * failure. The audit log table also carries the row — this banner
   * is the fast operator-feedback path. */
  .email-outcome {
    margin: 0 0 var(--space-3) 0;
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-surface-divider);
    border-left-width: 4px;
    border-radius: var(--radius-md, 6px);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }
  .email-outcome--succeeded {
    border-left-color: var(--color-signal-positive);
  }
  .email-outcome--failed {
    border-left-color: var(--color-signal-negative);
  }
  .email-outcome strong {
    display: block;
    margin-bottom: var(--space-1);
  }
  .email-outcome__detail {
    margin: 0;
    font-size: var(--type-size-1);
    color: var(--color-text-secondary);
  }

  /* PR-80 / session-102 — inline storno confirm panel. Sits below the
   * action bar with a clear destructive-action affordance. The
   * left-border accent in the negative-signal colour signals
   * "irreversible" without screaming. */
  .storno-confirm {
    margin: 0 0 var(--space-4) 0;
    padding: var(--space-3) var(--space-4);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-negative);
    border-left: 4px solid var(--color-signal-negative);
    border-radius: var(--radius-md, 6px);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .storno-confirm-title {
    margin: 0 0 var(--space-2) 0;
    color: var(--color-signal-negative);
    font-family: var(--type-family-body);
    font-size: var(--type-size-md);
    font-weight: 600;
  }

  .storno-confirm-grid {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-sm);
  }

  .storno-confirm-grid dt {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
  }

  .storno-confirm-grid dd {
    margin: 0;
    color: var(--color-text-strong);
  }

  .storno-confirm-copy {
    margin: 0 0 var(--space-3) 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: var(--type-line-normal);
  }

  .storno-confirm-copy strong {
    color: var(--color-signal-negative);
  }

  .storno-confirm-actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  /* PR-83 / session-103 — buyer-facing storno reason input. Sits
   * between the explanatory copy and the action buttons so the
   * operator confirms with the reason visible. Matches the pay-form
   * input typography for visual consistency within the modal. */
  .storno-reason-label {
    display: flex;
    flex-direction: column;
    gap: 0;
    margin: 0 0 var(--space-1) 0;
  }

  .storno-reason-label-text {
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
  }

  .storno-reason-hint {
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .storno-reason-input {
    width: 100%;
    padding: var(--space-1) var(--space-2);
    margin: 0 0 var(--space-3) 0;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    line-height: var(--type-line-normal);
    border: 1px solid var(--color-surface-divider);
    border-radius: 2px;
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    resize: vertical;
    box-sizing: border-box;
  }

  /* PR-44ε.UI — inline download-error message. Same `.error` styling
   * as the body-level load-error message; an extra `download-error`
   * modifier reduces top margin since the message sits directly under
   * the header rather than in the body-load region. */
  .error.download-error {
    margin: 0 0 var(--space-3) 0;
  }

  /* PR-58 / session-78 — typed NAV upstream-fault inline panel. Distinct
   * from the plain string-error panel by virtue of its dt/dd grid;
   * operator triages the fault by scanning fault_code / fault_message
   * top-down without parsing a wall-of-text. */
  .nav-fault {
    border: 1px solid var(--color-signal-negative);
    border-radius: var(--radius-md, 6px);
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface-sunken);
  }

  .nav-fault-headline {
    margin: 0 0 var(--space-2) 0;
    font-family: var(--type-family-base);
    font-weight: 600;
  }

  .nav-fault-grid {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0;
    font-family: var(--type-family-base);
  }

  .nav-fault-grid dt {
    color: var(--color-text-secondary);
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
    align-self: start;
  }

  .nav-fault-grid dd {
    margin: 0;
    color: var(--color-text-strong);
  }

  .nav-fault-body {
    margin: var(--space-1) 0 0 0;
    padding: var(--space-1) var(--space-2);
    background: var(--color-surface);
    border-radius: var(--radius-sm, 3px);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 12em;
    overflow: auto;
  }

  /* PR-59 / session-79 — per-rule technical_validations list. Rendered
   * BELOW the top-level fault_code / fault_message pair because NAV's
   * generic INVALID_REQUEST wrapper is rarely actionable on its own —
   * the operator's actual triage lives in the validation messages. */
  .nav-fault-body-details {
    margin-top: var(--space-2);
  }

  .nav-fault-body-details summary {
    cursor: pointer;
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  .nav-fault-validations-heading {
    margin: var(--space-3) 0 var(--space-1) 0;
    color: var(--color-text-secondary);
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
  }

  .nav-fault-validations {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .nav-fault-validation {
    padding: var(--space-2);
    background: var(--color-surface);
    border-radius: var(--radius-sm, 3px);
    border-left: 3px solid var(--color-signal-negative);
  }

  .nav-fault-validation-head {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    margin-bottom: var(--space-1);
  }

  .nav-fault-validation-code {
    font-size: var(--type-size-sm);
    color: var(--color-text-strong);
    font-weight: 600;
  }

  .nav-fault-validation-result {
    font-size: var(--type-size-xs);
    padding: 0 var(--space-1);
    border-radius: var(--radius-sm, 3px);
  }

  .nav-fault-validation-result.error {
    color: var(--color-signal-negative);
    background: var(--color-signal-negative-bg, transparent);
  }

  .nav-fault-validation-result.warn {
    color: var(--color-signal-caution, var(--color-text-secondary));
  }

  .nav-fault-validation-message {
    margin: 0;
    color: var(--color-text-strong);
  }

  .nav-fault-validation-tag {
    margin: var(--space-1) 0 0 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    word-break: break-all;
  }

  /* Two-column dt/dd grid for the invoice metadata. */
  .meta-grid {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-2) var(--space-4);
    margin: 0 0 var(--space-5) 0;
    font-size: var(--type-size-sm);
  }

  .meta-grid dt {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
    align-self: center;
  }

  .meta-grid dd {
    margin: 0;
    color: var(--color-text-strong);
  }

  .section-head {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    font-weight: 500;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
  }

  .muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
    margin: 0 0 var(--space-3) 0;
  }

  .error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: var(--space-2) 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  /* Dense table — same pattern as InvoiceList per ADR-0017 §3. */
  table.dense {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-md);
    background: var(--color-surface-sunken);
  }

  table.dense thead th {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    font-weight: 500;
    font-size: var(--type-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  table.dense tbody td {
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
  }

  table.dense tbody tr:hover {
    background: var(--color-surface-raised);
  }

  .mono {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
  }

  .col-num {
    text-align: right;
    width: 6ch;
  }

  .col-kind {
    /* Pre-PR-26 the column was a fixed 22ch (longest kind name).
     * PR-26 lets chain-link rows append `→ <base_id>` (~30 chars),
     * so the column grows to fit while keeping the 22ch floor so
     * non-chain rows still align with the rest of the dense table.
     * `word-break: break-all` lets long ULID-style base ids wrap if
     * the modal is narrowed. */
    min-width: 22ch;
    word-break: break-all;
  }

  /* PR-26 — chain-link affordance inside the kind cell. Same quiet-
   * link aesthetic as InvoiceList's id-link (per ADR-0017 §1-2 —
   * chrome stays quiet; underline-on-hover is the signal). */
  .id-link {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: var(--color-text-primary);
    text-align: left;
    cursor: pointer;
  }

  .id-link:hover,
  .id-link:focus-visible {
    color: var(--color-text-strong);
    text-decoration: underline;
    text-decoration-color: var(--color-text-muted);
    text-underline-offset: 2px;
  }

  .id-link:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  .chain-arrow {
    color: var(--color-text-muted);
    margin: 0 var(--space-1);
  }

  /* PR-41 — per-row chain index glyph. Quiet leading `#N` mono
   * prefix that pins the row to its position in the base's chain.
   * Same secondary-text colour as `.chain-arrow` per ADR-0017
   * §1-2 (chrome stays quiet; the affordance is the id-link to
   * the right). */
  .chain-index-prefix {
    color: var(--color-text-muted);
    margin-right: var(--space-2);
  }

  /* PR-32 — chain-children list. Quiet column of `<kind> →
   * <invoice_id>` rows between the meta-grid and the audit-trail
   * table. Aesthetic mirrors the audit-row chain-link affordance
   * (`.id-link` + `.chain-arrow`) so the operator recognises the
   * same chain semantics on both surfaces. Per ADR-0017 §1-2 the
   * chrome stays quiet; the affordance is the underline-on-hover
   * id-link. */
  .chain-children-list {
    list-style: none;
    margin: 0 0 var(--space-5) 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--type-size-sm);
  }

  /* PR-82 — buyer-facing notes section. Plain-text rendering matches
   * the printed-PDF surface; the operator's preview of what the
   * buyer sees. Whitespace preserved so multi-line notes look right. */
  .invoice-note-text {
    white-space: pre-wrap;
    margin: 0 0 var(--space-4) 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }

  .line-notes-list {
    list-style: none;
    margin: 0 0 var(--space-5) 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--type-size-sm);
  }

  .line-note-prefix {
    color: var(--color-text-muted);
    margin-right: var(--space-1);
  }

  .line-note-text {
    white-space: pre-wrap;
    color: var(--color-text-primary);
  }

  /* PR-27 — disclosure-triangle toggle for the per-row payload
   * drill-down. Same quiet-button aesthetic as `.id-link` and
   * `.quiet-button` per ADR-0017 §1-2 (chrome stays quiet; the
   * affordance is the glyph and the hover-strengthening). Sized so
   * the touch-target is reasonable without disturbing the dense
   * table's row height. */
  .expand-toggle {
    background: none;
    border: none;
    padding: 0;
    margin: 0 var(--space-1) 0 0;
    font: inherit;
    color: var(--color-text-muted);
    cursor: pointer;
    width: 1.25em;
    text-align: center;
  }

  .expand-toggle:hover,
  .expand-toggle:focus-visible {
    color: var(--color-text-strong);
  }

  .expand-toggle:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  /* PR-67 / session-89 — "Show raw table" toggle beneath the
   * timeline. Quiet-link aesthetic per ADR-0017 §1-2 (chrome stays
   * quiet; the affordance is the underline-on-hover). Sits flush-
   * left under the timeline with a small top margin so the
   * timeline's last node and the toggle do not collide. */
  .raw-table-toggle {
    background: none;
    border: none;
    padding: 0;
    margin: 0 0 var(--space-3) 0;
    font: inherit;
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    cursor: pointer;
  }

  .raw-table-toggle:hover,
  .raw-table-toggle:focus-visible {
    color: var(--color-text-strong);
    text-decoration: underline;
    text-decoration-color: var(--color-text-muted);
    text-underline-offset: 2px;
  }

  .raw-table-toggle:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 2px;
  }

  /* PR-27 — drill-down sub-row. Sunken background distinguishes
   * it from the regular audit row above; the JSON pre uses the
   * same monospace family as the rest of the table but a slightly
   * smaller size so long payloads stay inspectable without
   * dominating the modal. `overflow-x: auto` lets a wide line
   * (e.g., a long request_xml byte array) scroll horizontally
   * within the cell rather than forcing the dialog to grow. */
  .payload-row td {
    background: var(--color-surface-sunken);
    padding: var(--space-2) var(--space-3);
  }

  .payload-json {
    margin: 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    line-height: var(--type-line-normal);
    color: var(--color-text-secondary);
    white-space: pre;
    overflow-x: auto;
    max-height: 320px;
    overflow-y: auto;
  }

  .col-actor {
    width: 16ch;
  }

  .col-time {
    /* RFC3339 strings are ~25 chars; let the column take the rest. */
    width: auto;
  }

  .state-pill {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 0 var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    line-height: 1.6;
    letter-spacing: 0.04em;
    border: 1px solid var(--color-surface-divider);
    border-radius: 2px;
    background: var(--color-surface-base);
    color: var(--color-text-secondary);
    cursor: help;
  }

  .state-icon {
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    line-height: 1;
  }

  .state-pill.signal-positive {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }
  .state-pill.signal-negative {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .state-pill.signal-warning {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  .state-pill.signal-divergence {
    color: var(--color-signal-divergence);
    border-color: var(--color-signal-divergence);
  }
  .state-pill.signal-muted {
    color: var(--color-text-muted);
    border-color: var(--color-surface-divider);
  }

  /* PR-70 / ADR-0039 §2 — operational "Paid" badge sits next to the
     regulatory state chip. Distinct positive-signal colour to read
     as "operationally complete" without colliding with the
     `signal-positive` ack-status chip (which signals NAV-side SAVED
     terminal). The two chips render side-by-side; the small gap is
     inherited from the parent dd's whitespace. */
  .state-pill.paid-pill {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
    margin-left: var(--space-1);
  }

  /* PR-70 / ADR-0039 — mark-as-paid form modal. Compact form layout;
     each row is a `<label>` with a leading caption + an aligned
     control. Reuses the existing `.error` colour for the inline
     submit failure. */
  .pay-dialog {
    /* Native <dialog> wraps a sizing-by-content body; the form's
       width is constrained by the inputs' default min-width so the
       modal stays narrow. */
    width: min(420px, 90vw);
    padding: var(--space-4);
  }
  .pay-form {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .pay-title {
    margin: 0;
    font-family: var(--type-family-body);
    font-size: var(--type-size-md);
  }
  .pay-subtitle {
    margin: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
  }
  .pay-row {
    display: grid;
    grid-template-columns: 1fr;
    gap: var(--space-1);
  }
  .pay-label {
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }
  .pay-row input,
  .pay-row select {
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    border: 1px solid var(--color-surface-divider);
    border-radius: 2px;
    background: var(--color-surface-base);
    color: var(--color-text-primary);
  }
  .pay-row input[readonly] {
    color: var(--color-text-secondary);
    cursor: default;
  }
  .pay-error {
    margin: 0;
    font-size: var(--type-size-xs);
  }
  .pay-actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-2);
  }
</style>
