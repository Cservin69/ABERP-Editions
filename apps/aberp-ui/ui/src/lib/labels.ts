// PR-24 / ADR-0036 §7 — Svelte shell affordances for the eleven
// `derive_state` labels emitted by `apps/aberp/src/serve.rs`.
//
// This module is the SINGLE SOURCE OF TRUTH on the SPA side for
// per-label display affordances: signal colour, icon glyph, hover
// tooltip text, and lifecycle-natural sort order. Components MUST
// import from here; hardcoded label strings or per-label colour
// classes in components are a code-review block (same posture as
// `tokens.css` per ADR-0017 §"Tokens are the enforcement mechanism").
//
// The mirror invariant per ADR-0036 §1: `derive_state` is the
// authoritative source of label strings. Drift between this module
// and `serve.rs` surfaces as a `npm run check` failure (the
// `Record<InvoiceState, LabelMeta>` table is exhaustive over the
// `InvoiceState` union in `./api.ts`, which itself mirrors the
// eleven labels per ADR-0036 §2) plus a module-load runtime throw
// (the `LIFECYCLE_ORDER` length / dedup checks) per CLAUDE.md rule
// 12 (fail loud).

import type { AckStatus, InvoiceState } from "./api";

/** Categorical signal classes from ADR-0017 §"Token namespaces"
 * `color.signal.*`. The five values map to the existing CSS custom
 * properties in `tokens.css`; the violet (`divergence`) is
 * RESERVED per ADR-0017 §5 for ABERP↔NAV record disagreement —
 * `PendingNavExists` is the exact operator-visible case (ABERP
 * thinks Pending, NAV's Layer-2 check says Exists). */
export type LabelSignal =
  | "positive"
  | "negative"
  | "warning"
  | "divergence"
  | "muted";

/** Display affordance for one of the eleven labels per ADR-0036 §2. */
export interface LabelMeta {
  /** Maps to `.state-pill.signal-<x>` in `InvoiceList.svelte`. */
  signal: LabelSignal;
  /** Unicode glyph rendered before the label text per ADR-0017
   * §"Adversarial review #4" — categorical signals are never carried
   * by colour alone; every state has a glyph or label addition. */
  icon: string;
  /** One-sentence operator-readable summary of the trigger
   * condition. Rendered as the `title` attribute on the pill per
   * ADR-0036 §"Adversarial review" #5 (in-UI documentation benefits
   * the inspector use case). */
  tooltip: string;
}

/** The full eleven-label table. The `Record<InvoiceState, LabelMeta>`
 * shape is the load-bearing TS-level enforcement: if `InvoiceState`
 * adds a member (new ADR extends the ladder) and this table is not
 * updated in lockstep, `npm run check` fails on the missing key per
 * ADR-0036 §10's four-edit obligation at the Svelte side. */
export const LABELS: Record<InvoiceState, LabelMeta> = {
  Unknown: {
    signal: "muted",
    icon: "?",
    tooltip: "No audit-ledger entries for this invoice id.",
  },
  Ready: {
    signal: "muted",
    icon: "◇",
    tooltip:
      "Draft exists in the ledger; no submission attempted yet.",
  },
  Pending: {
    signal: "warning",
    icon: "⧖",
    tooltip:
      "Submission attempted; no response received from NAV; not abandoned. Consider 'aberp retry-submission'.",
  },
  PendingNavExists: {
    signal: "divergence",
    icon: "⚠",
    tooltip:
      "Pending locally, but NAV's Layer-2 check reports the invoice exists. Consider 'aberp recover-from-nav' to record the transactionId before retrying.",
  },
  Submitted: {
    signal: "warning",
    icon: "⇧",
    tooltip:
      "Response received from NAV; awaiting SAVED or ABORTED ack. Consider 'aberp poll-ack'.",
  },
  Recovered: {
    signal: "warning",
    icon: "↺",
    tooltip:
      "State reconstructed from NAV via 'aberp recover-from-nav' (queryInvoiceData response, not original-witness path).",
  },
  Finalized: {
    signal: "positive",
    icon: "✓",
    tooltip: "NAV ack received: SAVED.",
  },
  Rejected: {
    signal: "negative",
    icon: "✗",
    tooltip: "NAV ack received: ABORTED.",
  },
  Storno: {
    signal: "warning",
    icon: "⊘",
    tooltip:
      "Base invoice reversed by a storno chain entry. Audit entries view exposes the chain.",
  },
  Amended: {
    signal: "warning",
    icon: "✎",
    tooltip:
      "Base invoice modified by a modification chain entry. Audit entries view exposes the chain.",
  },
  Abandoned: {
    signal: "negative",
    icon: "⊗",
    tooltip:
      "Operator marked the invoice as abandoned (terminal-by-operator-decision).",
  },
};

/** Lifecycle-natural display order per ADR-0036 §3's priority
 * ladder, inverted from ladder-priority (Abandoned-wins-over-everything)
 * to lifecycle-progress (Unknown → ... → Abandoned). The session-27
 * handoff §"Suggested next session sub-split" lean was
 * lifecycle-natural over alphabetical — mirrors the operator's
 * mental model. */
export const LIFECYCLE_ORDER = [
  "Unknown",
  "Ready",
  "Pending",
  "PendingNavExists",
  "Submitted",
  "Recovered",
  "Finalized",
  "Rejected",
  "Storno",
  "Amended",
  "Abandoned",
] as const satisfies readonly InvoiceState[];

// ─── Module-load invariants (CLAUDE.md rule 12 — fail loud) ──────────
//
// TS-level coverage is already enforced by the `Record<InvoiceState,
// LabelMeta>` shape on `LABELS`. The runtime asserts below pin two
// further invariants that are NOT captured at the type level:
//
//  1. `LIFECYCLE_ORDER` lists every `InvoiceState` member exactly
//     once. A future PR that adds a label MUST add it to both
//     `LABELS` (TS-enforced) and `LIFECYCLE_ORDER` (this runtime
//     check) — adding only to `LABELS` would be a silent miss
//     (sortable, but not in lifecycle position).
//
//  2. `LIFECYCLE_ORDER` and `LABELS` agree on the label set. A
//     duplicate or typo in `LIFECYCLE_ORDER` fires loud at app
//     startup rather than silently mis-sorting in production.
{
  const labelKeys = Object.keys(LABELS) as InvoiceState[];
  const seen = new Set<InvoiceState>();
  for (const s of LIFECYCLE_ORDER) {
    if (seen.has(s)) {
      throw new Error(
        `labels.ts: LIFECYCLE_ORDER contains duplicate '${s}'`,
      );
    }
    seen.add(s);
  }
  if (seen.size !== labelKeys.length) {
    throw new Error(
      `labels.ts: LIFECYCLE_ORDER has ${seen.size} entries but LABELS has ${labelKeys.length}; one is out of date`,
    );
  }
  for (const k of labelKeys) {
    if (!seen.has(k)) {
      throw new Error(
        `labels.ts: LABELS member '${k}' is missing from LIFECYCLE_ORDER`,
      );
    }
  }
}

/** Stable sort index for an arbitrary state string. Known labels
 * sort by `LIFECYCLE_ORDER`; unknown labels (backend invented a new
 * state without a SPA update) sort AFTER every known label so they
 * remain visible at the bottom of the table rather than silently
 * bucketing with `Unknown`. Per CLAUDE.md rule 12 — unknown states
 * are visible, not hidden. */
export function lifecycleIndex(state: InvoiceState | string): number {
  const i = (LIFECYCLE_ORDER as readonly string[]).indexOf(state);
  return i === -1 ? LIFECYCLE_ORDER.length : i;
}

/** Display affordance lookup with a muted fallback for unknown
 * labels. The fallback tooltip names the divergence loud so an
 * inspector can tell the backend invented a string the SPA does
 * not model. */
export function labelMeta(state: InvoiceState | string): LabelMeta {
  const known = (LABELS as Record<string, LabelMeta | undefined>)[state];
  if (known !== undefined) {
    return known;
  }
  return {
    signal: "muted",
    icon: "?",
    tooltip: `Unknown label '${state}'. The backend emitted a state this SPA does not model — update labels.ts and api.ts.`,
  };
}

// ─── PR-36 / session-40 — Ack-status label table (Option Y) ──────────
//
// `AckStatus` and `InvoiceState` are disjoint concept domains: the
// former carries NAV's `processingResult` literals (per ADR-0009 §2),
// the latter the eleven derive_state labels per ADR-0036 §2. They
// share zero members, so `ACK_LABELS` is forked as its own
// `Record<AckStatus, LabelMeta>` table next to `LABELS` rather than
// widening the latter to a union key — that widening would conflate
// two domains and break the `LIFECYCLE_ORDER` invariant (which only
// makes sense for InvoiceState). The two surfaces share `LabelMeta`
// itself, so the chip CSS classes (.state-pill.signal-*) and the
// shape contract on the renderer are byte-identical to the State row
// per CLAUDE.md rule 11 (match conventions).
//
// Icon choices: SAVED and ABORTED reuse the same glyphs as the
// terminal `Finalized` / `Rejected` lifecycle states (per the
// existing tooltips on those labels, they ARE the SAVED / ABORTED
// outcomes). The two intermediate values get distinct glyphs so the
// operator can tell at a glance which NAV-side stage a Submitted
// invoice is at — `Submitted` collapses both per ADR-0036 §2.
//   - RECEIVED → `⇣` (mirrors `Submitted`'s `⇧` — request out,
//     ack in)
//   - PROCESSING → `⟳` (NAV-side work in progress; distinct from
//     the local-side `⧖` Pending hourglass)
//   - SAVED → `✓` (same glyph as `Finalized`)
//   - ABORTED → `✗` (same glyph as `Rejected`)
export const ACK_LABELS: Record<AckStatus, LabelMeta> = {
  RECEIVED: {
    signal: "warning",
    icon: "⇣",
    tooltip:
      "NAV acknowledged receipt; parsing not yet complete. Awaiting PROCESSING and then SAVED or ABORTED.",
  },
  PROCESSING: {
    signal: "warning",
    icon: "⟳",
    tooltip:
      "NAV started processing the submission; awaiting terminal ack (SAVED or ABORTED).",
  },
  SAVED: {
    signal: "positive",
    icon: "✓",
    tooltip:
      "NAV terminal ack: invoice stored. Equivalent to the Finalized lifecycle state.",
  },
  ABORTED: {
    signal: "negative",
    icon: "✗",
    tooltip:
      "NAV terminal ack: invoice rejected. Equivalent to the Rejected lifecycle state.",
  },
};

/** Mirror of `labelMeta` for the AckStatus surface. Muted-"?"
 * fallback for unknown strings per CLAUDE.md rule 12 — a persisted
 * ack literal the SPA does not model surfaces as a visible
 * divergence rather than silently bucketing with a known value. */
export function ackLabelMeta(status: AckStatus | string): LabelMeta {
  const known = (ACK_LABELS as Record<string, LabelMeta | undefined>)[status];
  if (known !== undefined) {
    return known;
  }
  return {
    signal: "muted",
    icon: "?",
    tooltip: `Unknown ack '${status}'. The backend emitted a NAV processingResult this SPA does not model — update labels.ts and api.ts.`,
  };
}
