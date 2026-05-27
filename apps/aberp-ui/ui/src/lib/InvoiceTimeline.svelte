<script lang="ts">
  // PR-67 / session-89 — vertical lifecycle timeline. Replaces the
  // dense audit-row table the pre-PR-67 InvoiceDetail modal
  // rendered. The component is presentational only — every
  // kind-dispatch decision (glyph, kind_class, label, body lines)
  // happens in `./invoice-timeline.ts`, pinned by vitest. The
  // operator-meaningful narrative reads top-to-bottom (chronological
  // per the audit-ledger's append-only seq order).
  //
  // CSS-only — no new dependencies. The left rail is a single ~2px
  // vertical line; each node punches a circular glyph badge into
  // the rail at its vertical position; the body content (heading,
  // timestamp, secondary lines) renders to the right of the badge.
  // Per-kind badge colouring routes through the kind_class CSS
  // class (the pure module's contract); the warm-charcoal +
  // signal-colour token namespace from ADR-0017 / tokens.css is
  // the single source of truth.

  import type { TimelineNode } from "./invoice-timeline";

  interface Props {
    nodes: TimelineNode[];
  }

  let { nodes }: Props = $props();
</script>

{#if nodes.length === 0}
  <p class="empty-state">No audit entries yet.</p>
{:else}
  <ol class="timeline" aria-label="Invoice lifecycle timeline">
    {#each nodes as node (node.id)}
      <li class="timeline-node">
        <span
          class="timeline-badge {node.kind_class}"
          aria-hidden="true"
        >
          {node.glyph}
        </span>
        <div class="timeline-body">
          <div class="timeline-head">
            <span class="timeline-label">{node.label_html_safe}</span>
            <time class="timeline-time mono" datetime={node.ts_iso}>
              {node.ts_display}
            </time>
          </div>
          {#if node.body_lines.length > 0}
            <ul class="timeline-meta">
              {#each node.body_lines as line, i (i)}
                <li class="mono">{line}</li>
              {/each}
            </ul>
          {/if}
        </div>
      </li>
    {/each}
  </ol>
{/if}

<style>
  /* Empty-state copy — quiet, muted, matches the per-page muted
   * paragraph pattern used elsewhere in the SPA. */
  .empty-state {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
    margin: var(--space-2) 0;
  }

  /* The timeline itself is a plain ordered list; we override list
   * chrome and lean on the ::before vertical rail for the visual
   * spine. `position: relative` anchors the rail to the list's
   * own left edge. */
  .timeline {
    list-style: none;
    margin: 0 0 var(--space-5) 0;
    padding: 0;
    position: relative;
  }

  /* The rail itself — a single ~2px vertical line behind every
   * badge. Drawn via ::before so it stays in the background
   * without affecting layout flow. Stops short of the first/last
   * badge centres via inset top/bottom so it does not poke out
   * past the end caps. */
  .timeline::before {
    content: "";
    position: absolute;
    left: 11px;
    top: 12px;
    bottom: 12px;
    width: 2px;
    background: var(--color-surface-divider);
  }

  /* Each node lays out as `badge | body` with the badge anchored
   * to the rail's column. The vertical gap between nodes is the
   * primary rhythm — wide enough to read each entry cleanly. */
  .timeline-node {
    position: relative;
    display: grid;
    grid-template-columns: 24px 1fr;
    gap: var(--space-3);
    padding: var(--space-2) 0;
    align-items: flex-start;
  }

  /* The circular glyph badge punching into the rail. The
   * background covers the rail line behind the badge so the
   * glyph appears to sit ON the rail, not behind it. */
  .timeline-badge {
    width: 24px;
    height: 24px;
    border-radius: 50%;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: var(--color-surface-base);
    border: 1px solid var(--color-surface-divider);
    color: var(--color-text-secondary);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    line-height: 1;
    /* Pull the badge slightly up so the rail centres on its
     * vertical midpoint, not its top edge. */
    margin-top: 2px;
    flex-shrink: 0;
    position: relative;
    z-index: 1;
  }

  /* Per-kind badge colouring. Each kind_class maps to one signal-
   * colour from tokens.css so the categorical signal stays the
   * single source of truth. */
  .timeline-badge.kind-issued {
    color: var(--color-text-strong);
    border-color: var(--color-text-secondary);
  }
  .timeline-badge.kind-submitted {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  .timeline-badge.kind-ack-saved {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }
  .timeline-badge.kind-ack-processing {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  .timeline-badge.kind-ack-received {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  .timeline-badge.kind-ack-aborted {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .timeline-badge.kind-storno {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  .timeline-badge.kind-modified {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  /* PR-70 / ADR-0039 §2 — operational paid badge. Positive-signal
     colour same as the regulatory SAVED-ack badge but a separate
     class so the two surfaces stay distinguishable. */
  .timeline-badge.kind-paid {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }
  .timeline-badge.kind-default {
    color: var(--color-text-muted);
    border-color: var(--color-surface-divider);
  }

  /* Right-side body — heading + timestamp on the top line, secondary
   * lines (actor, chain base id) below. */
  .timeline-body {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    min-width: 0;
  }

  .timeline-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    flex-wrap: wrap;
  }

  .timeline-label {
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
    font-weight: 500;
  }

  .timeline-time {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
  }

  .timeline-meta {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
  }

  .mono {
    font-family: var(--type-family-mono);
    font-variant-numeric: tabular-nums;
  }
</style>
