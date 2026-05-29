// Session 158 — live audit-entries poll for the InvoiceDetail modal.
//
// Ervin's ask: pressing "Issue invoice" navigates straight to the
// just-issued invoice's detail view, and the NAV pictogram + email
// status progress LIVE as the post-issue background tail (auto-submit
// → poll → SAVED/ABORTED, plus the auto-send-to-buyer email) lands its
// audit-ledger entries. The detail view already DERIVES the pictogram
// (PR-95) and the email-sent status (PR-99) reactively from
// `detail.audit_entries`; the only missing piece is re-fetching the
// detail while the invoice is still in flight so those derivations see
// fresh data without an operator click.
//
// This module is the pure, side-effect-isolated brain of that loop:
//   - the terminal-state predicates (`isNavTerminal`, `isEmailSettled`,
//     `isPollTerminal`) decide WHEN to stop, and
//   - `createDetailPoller` owns the interval lifecycle (start / stop /
//     5-minute cap) over INJECTED timer + clock functions so vitest can
//     pin every branch with fake timers and a mock refetch.
//
// The Svelte component (`InvoiceDetail.svelte`) supplies the real
// `refetch` (which calls its `load()` then reads the fresh `detail`),
// the real `setInterval`/`clearInterval`/`Date.now`, and wires
// `start()` after the first load + `stop()` on navigation / destroy.
// Keeping the decision logic out of the `.svelte` file mirrors the
// codebase's pure-lib + vitest convention (CLAUDE.md rule 11) — there
// is no component-mount test harness in this repo.

import type { NavPictogramState } from "./nav-status-pictogram";

/** Poll cadence while the invoice is non-terminal. ~2s per the brief —
 * fast enough to feel live, slow enough not to hammer the local
 * backend (the round-trip is a single in-process `getInvoice`). */
export const POLL_INTERVAL_MS = 2000;

/** Hard cap on how long the loop runs without reaching a terminal
 * state. The post-issue tail is bounded (ADR-0009 §5 poll ≤ 31s + an
 * SMTP send), so a non-terminal invoice past this window is genuinely
 * stuck (NAV still PROCESSING, or a submission the daemon-poll
 * architecture — a different session's queue — would own). We stop and
 * log a warning rather than poll forever; the operator can manually
 * refresh or click the actionable pictogram. */
export const POLL_CAP_MS = 5 * 60 * 1000;

/** The minimal snapshot the loop needs from a freshly-fetched invoice
 * to decide whether to keep polling. Derived by the component from the
 * reloaded `detail` (NAV pictogram state + presence of any email-send
 * audit entry). */
export interface DetailPollSnapshot {
  /** The four-state pictogram bucket from `navStatusPictogram(state)`. */
  navState: NavPictogramState;
  /** Whether an `InvoiceEmailedSent` audit entry exists yet — true once
   * the auto-send (or a manual send) has recorded its outcome, whether
   * the send SUCCEEDED or FAILED (both are terminal for the send). */
  hasEmailAttempt: boolean;
}

/** NAV side is terminal once the pictogram reaches a state a fresh poll
 * cannot change: `Final` (SAVED — or a SAVED-based Storno/Amended) or
 * `Rejected` (ABORTED / operator-abandoned). `NotSubmitted` and
 * `Submitted` are still in flight (the background tail, or a future
 * operator action, can still advance them). */
export function isNavTerminal(navState: NavPictogramState): boolean {
  return navState === "Final" || navState === "Rejected";
}

/** Email is settled when EITHER an email-send audit entry already
 * exists (`hasEmailAttempt` — succeeded or failed, both terminal for
 * the send) OR the NAV side has reached a terminal ack.
 *
 * The second clause closes the opted-out gap: the post-issue background
 * tail sends the email BEFORE it submits to NAV, so by the time NAV
 * reaches a terminal ack any email that was ever going to be sent has
 * already written its `InvoiceEmailedSent` row. A NAV-terminal invoice
 * with no email row therefore means the operator toggled auto-send off
 * — nothing more will arrive, so the loop must not wait on it (which
 * would otherwise burn the full 5-minute cap on every email-off
 * issuance). */
export function isEmailSettled(
  hasEmailAttempt: boolean,
  navTerminal: boolean,
): boolean {
  return hasEmailAttempt || navTerminal;
}

/** The loop halts once the NAV ack is terminal AND the email send is
 * settled — both gates, per the brief. */
export function isPollTerminal(snapshot: DetailPollSnapshot): boolean {
  const navTerminal = isNavTerminal(snapshot.navState);
  return navTerminal && isEmailSettled(snapshot.hasEmailAttempt, navTerminal);
}

/** Whether the elapsed wall-clock since the loop started has hit the
 * cap. Pulled out as a named predicate so the cap is pinned
 * independently of the interval machinery. */
export function isPollCapExceeded(elapsedMs: number): boolean {
  return elapsedMs >= POLL_CAP_MS;
}

/** Injected dependencies for the poller. The timer + clock functions
 * are parameters (not module imports) so tests drive them with fake
 * timers and a controllable clock. */
export interface DetailPollerDeps {
  /** Re-fetch the invoice and resolve to its fresh snapshot. A
   * rejection is tolerated (logged via `onError`, the loop continues to
   * the next tick) — a transient local fetch blip should not kill a
   * live view. */
  refetch: () => Promise<DetailPollSnapshot>;
  /** Called once when the cap is hit (the loop also stops). The
   * component logs a warning. */
  onCapExceeded?: () => void;
  /** Called when a `refetch` rejects. The loop keeps running. */
  onError?: (err: unknown) => void;
  intervalMs?: number;
  capMs?: number;
  now: () => number;
  setIntervalFn: (cb: () => void, ms: number) => ReturnType<typeof setInterval>;
  clearIntervalFn: (handle: ReturnType<typeof setInterval>) => void;
}

export interface DetailPoller {
  /** Begin polling. No-op if already running (idempotent — a second
   * `start` does not stack a second interval). */
  start: () => void;
  /** Stop polling and drop the interval. Idempotent — safe to call on
   * navigation AND on destroy without double-clearing. */
  stop: () => void;
  /** True between a `start` and its `stop` / terminal / cap. */
  isRunning: () => boolean;
}

/** Build a poller over injected timer + clock + refetch. The loop
 * fires `refetch` every `intervalMs`; it stops itself when the fetched
 * snapshot is terminal (`isPollTerminal`) or when `capMs` elapses since
 * `start`. A rejected refetch is swallowed (the loop survives a
 * transient blip). Re-entrancy is guarded: if a slow refetch is still
 * in flight when the next tick fires, the tick is skipped rather than
 * stacking overlapping fetches. */
export function createDetailPoller(deps: DetailPollerDeps): DetailPoller {
  const intervalMs = deps.intervalMs ?? POLL_INTERVAL_MS;
  const capMs = deps.capMs ?? POLL_CAP_MS;

  let handle: ReturnType<typeof setInterval> | null = null;
  let startedAt = 0;
  let inFlight = false;

  function stop(): void {
    if (handle !== null) {
      deps.clearIntervalFn(handle);
      handle = null;
    }
  }

  async function tick(): Promise<void> {
    // Skip if a prior refetch hasn't resolved yet (a slow round-trip
    // must not stack overlapping fetches that race on the component's
    // `detail` state).
    if (inFlight) return;
    // Cap check fires BEFORE the fetch so a stuck invoice stops on
    // schedule rather than one extra round-trip late. Compares against
    // the injectable `capMs` (tests shrink it); `isPollCapExceeded` is
    // the same comparison pinned against the production default.
    if (deps.now() - startedAt >= capMs) {
      stop();
      deps.onCapExceeded?.();
      return;
    }
    inFlight = true;
    try {
      const snapshot = await deps.refetch();
      if (isPollTerminal(snapshot)) {
        stop();
      }
    } catch (err) {
      deps.onError?.(err);
      // Loop continues — the next tick retries.
    } finally {
      inFlight = false;
    }
  }

  function start(): void {
    if (handle !== null) return; // already running
    startedAt = deps.now();
    handle = deps.setIntervalFn(() => {
      void tick();
    }, intervalMs);
  }

  return {
    start,
    stop,
    isRunning: () => handle !== null,
  };
}
