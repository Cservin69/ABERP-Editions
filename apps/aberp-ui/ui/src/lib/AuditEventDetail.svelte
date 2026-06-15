<script lang="ts">
  // S424 / session-424 — row-expansion modal for the Audit-events screen.
  // Lazy-fetches the single FULL entry (payload included) for `seq`, then
  // renders three things (design §4.6 + §5):
  //   1. Hash anchors — prev_hash → entry_hash + the per-row ✓/✗ hash_ok
  //      (the tamper-evidence surface; always visible — it is the point).
  //   2. Payload — REDACTED by default (Ervin 2026-06-15 #4): sensitive
  //      fields (*credential*/*password*/*token*/*secret*/*hash*) + large
  //      NAV-XML blobs masked. A "show raw" toggle reveals them but
  //      requires a per-use confirmation modal each time
  //      ([[trust-code-not-operator]] — defaults code-driven, never
  //      operator-trust-driven).
  //   3. Checklist sidebar — the subject's reconstructed state-machine
  //      path (✓ reached / ⏳ pending / ✗ failed), built from the kinds
  //      present for the subject among the loaded rows.
  import { getAuditEvent, type AuditEventDetail } from "../lib/api";
  import {
    checklistsForKinds,
    kindLabel,
    redactPayload,
    wouldRedact,
    type AuditEventRow,
  } from "../lib/audit-events-list";
  import { formatHungarianTimestamp } from "../lib/invoice-timeline";

  let {
    seq,
    rows,
    onClose,
  }: {
    seq: number | null;
    rows: AuditEventRow[];
    onClose: () => void;
  } = $props();

  type LoadState = "idle" | "loading" | "ready" | "error";
  let loadState = $state<LoadState>("idle");
  let detail = $state<AuditEventDetail | null>(null);
  let errorMessage = $state<string | null>(null);
  // showRaw is reset to false on every open — the operator must
  // re-confirm each time they reveal a sensitive payload.
  let showRaw = $state(false);
  let confirmRaw = $state(false);

  const now = new Date();

  // Fetch when `seq` changes. The effect reads `seq` (reactive); the
  // other state resets keep stale data off the screen during the fetch.
  $effect(() => {
    const s = seq;
    showRaw = false;
    confirmRaw = false;
    detail = null;
    if (s === null) {
      loadState = "idle";
      return;
    }
    loadState = "loading";
    errorMessage = null;
    getAuditEvent(s)
      .then((d) => {
        detail = d;
        loadState = "ready";
      })
      .catch((e) => {
        errorMessage = e instanceof Error ? e.message : String(e);
        loadState = "error";
      });
  });

  // The subject's state-machine checklists, reconstructed from the kinds
  // present for this subject among the LOADED rows (filtering the list to
  // `quote:X` loads the full subject set). The expanded entry's own kind
  // is always included.
  let checklists = $derived.by(() => {
    if (detail === null || detail.subject === null) return [];
    const subject = detail.subject;
    const kinds = new Set(
      rows.filter((r) => r.subject === subject).map((r) => r.kind),
    );
    kinds.add(detail.kind);
    return checklistsForKinds(kinds);
  });

  let redacted = $derived(
    detail === null ? null : redactPayload(detail.payload, detail.kind, showRaw),
  );
  let hasSensitive = $derived(
    detail === null ? false : wouldRedact(detail.payload, detail.kind),
  );

  function prettyPayload(v: unknown): string {
    try {
      return JSON.stringify(v, null, 2);
    } catch {
      return String(v);
    }
  }

  function timestampDisplay(iso: string): { display: string; absolute: string } {
    return formatHungarianTimestamp(iso, now);
  }

  function askShowRaw(): void {
    confirmRaw = true;
  }
  function confirmShowRaw(): void {
    showRaw = true;
    confirmRaw = false;
  }
  function cancelShowRaw(): void {
    confirmRaw = false;
  }
  function hideRaw(): void {
    showRaw = false;
  }

  function stepGlyph(status: string): string {
    switch (status) {
      case "reached":
        return "✓";
      case "failed":
        return "✗";
      default:
        return "⏳";
    }
  }
</script>

{#if seq !== null}
  <div class="aud-modal-backdrop" role="presentation" onclick={onClose}>
    <div
      class="aud-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="aud-detail-title"
      tabindex="-1"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => {
        if (e.key === "Escape") onClose();
      }}
      data-testid="audit-detail-modal"
    >
      <header class="aud-modal__hdr">
        <h3 id="aud-detail-title">
          {#if detail}{kindLabel(detail.kind)}{:else}Esemény / Event #{seq}{/if}
        </h3>
        <button
          type="button"
          class="btn btn--secondary"
          onclick={onClose}
          data-testid="audit-detail-close"
        >Bezárás / Close</button>
      </header>

      {#if loadState === "loading"}
        <p class="aud-modal__hint">Betöltés… / Loading…</p>
      {:else if loadState === "error"}
        <p class="aud-modal__err" data-testid="audit-detail-error">
          {errorMessage ?? "Hiba / Error"}
        </p>
      {:else if detail}
        {@const ts = timestampDisplay(detail.occurred_at)}
        <div class="aud-modal__body">
          <div class="aud-modal__main">
            <!-- Metadata -->
            <dl class="aud-meta">
              <div><dt>Seq</dt><dd><code>#{detail.seq}</code></dd></div>
              <div><dt>Kind</dt><dd><code>{detail.kind}</code></dd></div>
              <div>
                <dt>Időpont / When</dt>
                <dd><time datetime={detail.occurred_at} title={ts.absolute}>{ts.display}</time></dd>
              </div>
              <div><dt>Operátor / Operator</dt><dd>{detail.actor}</dd></div>
              {#if detail.subject}
                <div><dt>Tárgy / Subject</dt><dd><code>{detail.subject}</code></dd></div>
              {/if}
            </dl>

            <!-- Hash anchors (tamper-evidence) -->
            <section class="aud-hash" aria-label="Hash-lánc / Hash chain">
              <h4>
                Lánc / Chain
                <span
                  class={detail.hash_ok ? "aud-hash__ok" : "aud-hash__bad"}
                  data-testid="audit-detail-hash-ok"
                >{detail.hash_ok ? "✓ ép / intact" : "✗ sérült / tampered"}</span>
              </h4>
              <div class="aud-hash__row">
                <span class="aud-hash__lbl">prev_hash</span>
                <code class="aud-hash__hex">{detail.prev_hash_hex}</code>
              </div>
              <div class="aud-hash__row">
                <span class="aud-hash__lbl">entry_hash</span>
                <code class="aud-hash__hex">{detail.entry_hash_hex}</code>
              </div>
            </section>

            <!-- Payload (redacted by default) -->
            <section class="aud-payload" aria-label="Adattartalom / Payload">
              <div class="aud-payload__hdr">
                <h4>Adattartalom / Payload</h4>
                {#if hasSensitive}
                  {#if showRaw}
                    <button
                      type="button"
                      class="btn btn--secondary"
                      onclick={hideRaw}
                      data-testid="audit-detail-hide-raw"
                    >Elrejtés / Hide sensitive</button>
                  {:else}
                    <button
                      type="button"
                      class="btn btn--secondary"
                      onclick={askShowRaw}
                      data-testid="audit-detail-show-raw"
                    >Nyers megjelenítése / Show raw</button>
                  {/if}
                {/if}
              </div>
              {#if hasSensitive && !showRaw}
                <p class="aud-payload__note" data-testid="audit-detail-redacted-note">
                  Érzékeny mezők elrejtve (hitelesítők, jelszavak, tokenek,
                  hash-ek) és nagy NAV-XML blokkok. / Sensitive fields
                  (credentials, passwords, tokens, hashes) and large NAV-XML
                  blobs are hidden.
                </p>
              {/if}
              <pre class="aud-payload__json" data-testid="audit-detail-payload">{prettyPayload(redacted)}</pre>
            </section>
          </div>

          <!-- Checklist sidebar -->
          {#if checklists.length > 0}
            <aside class="aud-checklists" aria-label="Munkafolyamat / Workflow">
              <h4>Munkafolyamat / Workflow</h4>
              {#each checklists as cl (cl.id)}
                <div class="aud-checklist" data-testid={`audit-checklist-${cl.id}`}>
                  <div class="aud-checklist__title">{cl.label}</div>
                  <ol class="aud-checklist__steps">
                    {#each cl.steps as step, i (i)}
                      <li class={`aud-step aud-step--${step.status}`}>
                        <span class="aud-step__glyph" aria-hidden="true">{stepGlyph(step.status)}</span>
                        <span class="aud-step__label">{step.label}</span>
                      </li>
                    {/each}
                  </ol>
                </div>
              {/each}
            </aside>
          {/if}
        </div>
      {/if}
    </div>
  </div>
{/if}

<!-- Show-raw confirmation (Ervin #4 — per-use confirm). -->
{#if confirmRaw}
  <div class="aud-modal-backdrop aud-modal-backdrop--top" role="presentation" onclick={cancelShowRaw}>
    <div
      class="aud-confirm"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="aud-confirm-title"
      tabindex="-1"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => {
        if (e.key === "Escape") cancelShowRaw();
      }}
      data-testid="audit-detail-show-raw-modal"
    >
      <h3 id="aud-confirm-title">Érzékeny adat megjelenítése? / Show sensitive payload?</h3>
      <p>
        A nyers adattartalom hitelesítőket, jelszavakat, tokeneket vagy
        hash-eket tartalmazhat. Biztosan megjeleníted? / The raw payload may
        contain credentials, passwords, tokens, or hashes. Show it anyway?
      </p>
      <div class="aud-confirm__actions">
        <button
          type="button"
          class="btn btn--secondary"
          onclick={cancelShowRaw}
          data-testid="audit-detail-show-raw-cancel"
        >Mégse / No</button>
        <button
          type="button"
          class="btn btn--danger"
          onclick={confirmShowRaw}
          data-testid="audit-detail-show-raw-confirm"
        >Megjelenítés / Yes, show</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .aud-modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 50;
    padding: 16px;
  }
  .aud-modal-backdrop--top {
    z-index: 60;
  }
  .aud-modal {
    background: var(--color-surface, #1f2937);
    color: var(--color-text, #e5e7eb);
    border: 1px solid var(--color-border, #374151);
    border-radius: 8px;
    width: min(900px, 100%);
    max-height: 90vh;
    overflow: auto;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.5);
  }
  .aud-modal__hdr {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
    padding: 16px 20px;
    border-bottom: 1px solid var(--color-border, #374151);
    position: sticky;
    top: 0;
    background: var(--color-surface, #1f2937);
  }
  .aud-modal__hdr h3 {
    margin: 0;
    font-size: 16px;
  }
  .aud-modal__hint,
  .aud-modal__err {
    padding: 16px 20px;
  }
  .aud-modal__err {
    color: var(--color-danger, #f87171);
  }
  .aud-modal__body {
    display: flex;
    gap: 16px;
    padding: 16px 20px;
    flex-wrap: wrap;
  }
  .aud-modal__main {
    flex: 1 1 460px;
    min-width: 0;
  }
  .aud-meta {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 4px 16px;
    margin: 0 0 16px;
  }
  .aud-meta > div {
    display: contents;
  }
  .aud-meta dt {
    color: var(--color-text-muted, #9ca3af);
    font-size: 12px;
  }
  .aud-meta dd {
    margin: 0;
    font-size: 13px;
    word-break: break-all;
  }
  .aud-hash {
    border: 1px solid var(--color-border, #374151);
    border-radius: 6px;
    padding: 10px 12px;
    margin-bottom: 16px;
  }
  .aud-hash h4 {
    margin: 0 0 8px;
    font-size: 13px;
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .aud-hash__ok {
    color: var(--color-ok, #34d399);
    font-size: 12px;
  }
  .aud-hash__bad {
    color: var(--color-danger, #f87171);
    font-size: 12px;
  }
  .aud-hash__row {
    display: flex;
    gap: 8px;
    align-items: baseline;
    font-size: 11px;
  }
  .aud-hash__lbl {
    color: var(--color-text-muted, #9ca3af);
    min-width: 72px;
  }
  .aud-hash__hex {
    word-break: break-all;
    color: var(--color-text-muted, #9ca3af);
  }
  .aud-payload__hdr {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
  }
  .aud-payload__hdr h4 {
    margin: 0;
    font-size: 13px;
  }
  .aud-payload__note {
    font-size: 12px;
    color: var(--color-warn, #f59e0b);
    margin: 6px 0;
  }
  .aud-payload__json {
    background: var(--color-surface-2, #111827);
    border: 1px solid var(--color-border, #374151);
    border-radius: 6px;
    padding: 10px 12px;
    font-size: 12px;
    overflow: auto;
    max-height: 360px;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .aud-checklists {
    flex: 1 1 280px;
    min-width: 240px;
  }
  .aud-checklists h4 {
    margin: 0 0 8px;
    font-size: 13px;
  }
  .aud-checklist {
    border: 1px solid var(--color-border, #374151);
    border-radius: 6px;
    padding: 10px 12px;
    margin-bottom: 10px;
  }
  .aud-checklist__title {
    font-size: 12px;
    font-weight: 600;
    color: var(--color-text-muted, #9ca3af);
    margin-bottom: 6px;
  }
  .aud-checklist__steps {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .aud-step {
    display: flex;
    gap: 8px;
    align-items: baseline;
    font-size: 12px;
    padding: 2px 0;
  }
  .aud-step__glyph {
    min-width: 14px;
  }
  .aud-step--reached .aud-step__glyph {
    color: var(--color-ok, #34d399);
  }
  .aud-step--failed {
    color: var(--color-danger, #f87171);
  }
  .aud-step--failed .aud-step__glyph {
    color: var(--color-danger, #f87171);
  }
  .aud-step--pending {
    color: var(--color-text-muted, #9ca3af);
  }
  .aud-confirm {
    background: var(--color-surface, #1f2937);
    color: var(--color-text, #e5e7eb);
    border: 1px solid var(--color-border, #374151);
    border-radius: 8px;
    padding: 20px;
    max-width: 460px;
    width: calc(100% - 32px);
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.5);
  }
  .aud-confirm h3 {
    margin: 0 0 8px;
    font-size: 16px;
  }
  .aud-confirm p {
    margin: 8px 0;
    font-size: 13px;
  }
  .aud-confirm__actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .btn--danger {
    background: var(--color-danger, #7f1d1d);
    color: #fecaca;
    border-color: var(--color-danger, #b91c1c);
  }
  .btn--danger:hover:not(:disabled) {
    background: #991b1b;
  }
</style>
