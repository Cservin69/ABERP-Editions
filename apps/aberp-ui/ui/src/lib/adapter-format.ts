// S257 / PR-246 — pure closed-vocab formatting for the Adapters page.
//
// Extracted from AdaptersList/AdapterForm so the kind/status mappings
// are unit-testable without mounting a component (CLAUDE.md rule 9 —
// tests verify intent). The closed vocab here mirrors the Rust
// `AdapterKind` wire strings + the `adapter_health_status` vocab.

import type { AdapterKind, AdapterStatus, AdapterTransitionEntry } from "./api";

/** Display labels for each adapter kind, in picker display order. */
export const ADAPTER_KIND_LABELS: Record<AdapterKind, string> = {
  "barcode-scanner": "Barcode scanner",
  "label-printer": "Zebra label printer",
  "cnc-machine": "MTConnect CNC",
  robot: "UR RTDE robot",
  "laser-cutter": "Trumpf laser cutter",
};

/** Display order for the Add-wizard kind picker. */
export const ADAPTER_KIND_ORDER: AdapterKind[] = [
  "barcode-scanner",
  "label-printer",
  "cnc-machine",
  "robot",
  "laser-cutter",
];

/** Resolve a kind to its display label. Closed-vocab graceful degrade:
 * a SPA build older than the backend may see an unknown kind string —
 * render the raw value + log a console warning rather than crash (S257
 * forward-compat note). */
export function adapterKindLabel(kind: string): string {
  const label = ADAPTER_KIND_LABELS[kind as AdapterKind];
  if (label === undefined) {
    // eslint-disable-next-line no-console
    console.warn(`aberp: unknown adapter kind "${kind}" — rendering raw`);
    return kind;
  }
  return label;
}

/** Display labels for each live status. */
export const ADAPTER_STATUS_LABELS: Record<AdapterStatus, string> = {
  healthy: "Healthy",
  degraded: "Degraded",
  unhealthy: "Unhealthy",
  starting: "Starting",
  stopped: "Stopped",
};

export function adapterStatusLabel(status: string): string {
  return ADAPTER_STATUS_LABELS[status as AdapterStatus] ?? status;
}

/** Chip tone for a status. `degraded` + `starting` share the warning
 * tone (transitional / attention); an unknown status falls back to the
 * neutral `muted` tone rather than a misleading colour. */
export type AdapterStatusTone = "positive" | "warning" | "negative" | "muted";

export function adapterStatusTone(status: string): AdapterStatusTone {
  switch (status) {
    case "healthy":
      return "positive";
    case "degraded":
    case "starting":
      return "warning";
    case "unhealthy":
      return "negative";
    default:
      return "muted";
  }
}

// ── S258 / PR-247 — wall-TV health alerting ─────────────────────────

/** Closed-vocab test: is this status one the wall TV should ALARM on
 *  (red border + slow pulse, and — on the transition INTO it — a chime)?
 *  `degraded` + `unhealthy` alarm; `healthy` / `starting` / `stopped` do
 *  not. Mirrors the Rust `is_alerting_status` definition. Pure so the
 *  pulse class binds reactively to it (no JS interval driving the
 *  animation — that would fight Svelte's diffing). */
export function isAlertingState(status: string): boolean {
  return status === "degraded" || status === "unhealthy";
}

/** Per-session alert bookkeeping for the adapter chime. Kept outside the
 *  component so the logic is unit-testable. `acknowledgedSeq < 0` means
 *  "not yet seeded" — the FIRST poll of a page session seeds it to the
 *  newest transition seq WITHOUT chiming (boot-grace: a tile already
 *  alerting on load, or a transition that happened while the view was
 *  closed, must not fire). `lastChimeAtByAdapter` maps adapter_id → the
 *  ledger `at_iso8601` epoch-ms of its last chime, for the flapping
 *  debounce. */
export interface AdapterAlertState {
  acknowledgedSeq: number;
  lastChimeAtByAdapter: Record<string, number>;
}

export function initialAdapterAlertState(): AdapterAlertState {
  return { acknowledgedSeq: -1, lastChimeAtByAdapter: {} };
}

/** Default flapping-debounce window: ignore a second chime for the same
 *  adapter within 30s of the first (a CNC flapping
 *  healthy→unhealthy→healthy→unhealthy chimes once, not every cycle). */
export const ADAPTER_CHIME_DEBOUNCE_MS = 30_000;

/** PURE: given the recent ledger transitions and the prior alert state,
 *  decide which adapters should chime this poll and return the advanced
 *  state. Drives the chime off the AUDIT LEDGER (the `seq` high-water-
 *  mark + the `at_iso8601` debounce clock), so a page reload recovers
 *  from the ledger payload rather than from in-memory JS — a reload
 *  reseeds `acknowledgedSeq` to the newest transition present and so
 *  replays nothing.
 *
 *  Chimes only on a NON-alerting → alerting transition (Healthy/Starting/
 *  Stopped → Degraded/Unhealthy), NOT on a Degraded → Unhealthy
 *  escalation (the tile is already alerting + already pulsing). Returns
 *  the COUNT of distinct adapters that should chime; the caller plays a
 *  single coalesced tone when it is > 0. */
export function computeAdapterAlerts(
  transitions: AdapterTransitionEntry[],
  state: AdapterAlertState,
  debounceMs: number = ADAPTER_CHIME_DEBOUNCE_MS,
): { chimeCount: number; next: AdapterAlertState } {
  // Defensive copy + ascending sort so the high-water-mark advances
  // monotonically regardless of payload order.
  const sorted = [...transitions].sort((a, b) => a.seq - b.seq);
  const maxSeq = sorted.length > 0 ? sorted[sorted.length - 1].seq : -1;

  // First poll of the session: seed the high-water-mark, chime nothing.
  if (state.acknowledgedSeq < 0) {
    return {
      chimeCount: 0,
      next: {
        acknowledgedSeq: Math.max(0, maxSeq),
        lastChimeAtByAdapter: { ...state.lastChimeAtByAdapter },
      },
    };
  }

  const lastChimeAt = { ...state.lastChimeAtByAdapter };
  const chimed = new Set<string>();
  for (const t of sorted) {
    if (t.seq <= state.acknowledgedSeq) continue;
    if (!isAlertingState(t.to_state) || isAlertingState(t.from_state)) continue;
    const tMs = Date.parse(t.at_iso8601);
    const stamp = Number.isNaN(tMs) ? 0 : tMs;
    const prev = lastChimeAt[t.adapter_id];
    if (prev !== undefined && stamp - prev < debounceMs) continue;
    lastChimeAt[t.adapter_id] = stamp;
    chimed.add(t.adapter_id);
  }

  return {
    chimeCount: chimed.size,
    next: {
      acknowledgedSeq: Math.max(state.acknowledgedSeq, maxSeq),
      lastChimeAtByAdapter: lastChimeAt,
    },
  };
}
