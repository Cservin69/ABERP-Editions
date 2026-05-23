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
    getInvoice,
    type InvoiceDetail,
  } from "../lib/api";
  import {
    ackLabelMeta,
    labelMeta,
    type LabelSignal,
  } from "../lib/labels";
  import { bytesAsUtf8Replacer } from "../lib/payload-reviver";

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
  }

  let { invoiceId, ancestors, onClose, onNavigate, onJumpBack }: Props =
    $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  let detail: InvoiceDetail | null = $state(null);
  let loadState: "idle" | "loading" | "loaded" | "error" = $state("idle");
  let errorMessage: string | null = $state(null);
  // PR-27 — per-row expanded-payload state. Reassignment pattern
  // (build a new Set on every toggle) guarantees Svelte 5
  // reactivity without depending on Set-mutation tracking through
  // the $state proxy. The set is keyed by `entry.seq` because seq
  // is the audit-ledger's append-only primary key per ADR-0008 —
  // unique per ledger AND stable across the lifetime of the modal.
  let expandedSeqs: Set<number> = $state(new Set());

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
      void load(invoiceId);
    } else {
      if (dialogEl.open) dialogEl.close();
      detail = null;
      loadState = "idle";
      errorMessage = null;
      expandedSeqs = new Set();
    }
  });

  async function load(id: string) {
    loadState = "loading";
    errorMessage = null;
    detail = null;
    try {
      detail = await getInvoice(id);
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      errorMessage = err instanceof Error ? err.message : String(err);
    }
  }

  function signalClass(signal: LabelSignal): string {
    return `signal-${signal}`;
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

  const hufFormatter = new Intl.NumberFormat("hu-HU", {
    style: "currency",
    currency: "HUF",
    minimumFractionDigits: 0,
    maximumFractionDigits: 0,
  });

  function formatHuf(value: number | null): string {
    if (value === null) return "—";
    return hufFormatter.format(value);
  }

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
        <h2 class="detail-id mono">{invoiceId ?? ""}</h2>
      </div>
      <button
        type="button"
        class="quiet-button"
        onclick={() => dialogEl?.close()}
        aria-label="Close invoice detail"
      >
        Close
      </button>
    </header>

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
        <dt>State</dt>
        <dd>
          <span
            class="state-pill {signalClass(meta.signal)}"
            title={meta.tooltip}
          >
            <span class="state-icon" aria-hidden="true">{meta.icon}</span>
            <span class="state-text">{detail.state}</span>
          </span>
        </dd>
        <dt>Total (gross)</dt>
        <dd class="mono">{formatHuf(detail.total_gross)}</dd>
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
      </dl>

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
      {#if detail.audit_entries.length === 0}
        <p class="muted">
          No audit-ledger entries reference this invoice id directly.
          Chain-link entries (storno / modification) reference this
          invoice via their <code>base_invoice_id</code> payload field
          and do not appear in this list per <code>serve.rs</code>'s
          per-id walker.
        </p>
      {:else}
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
  </div>
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

  .detail-id {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
    word-break: break-all;
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

  .quiet-button:hover {
    color: var(--color-text-strong);
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

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }
</style>
