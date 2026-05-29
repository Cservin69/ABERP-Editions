// Session 158 — pins for the InvoiceDetail live-poll brain.
//
// Two layers:
//   1. The pure terminal predicates (`isNavTerminal`, `isEmailSettled`,
//      `isPollTerminal`, `isPollCapExceeded`) — each branch asserted
//      with a distinct truth-table row so a regression that collapsed
//      one to a constant cannot pass vacuously (CLAUDE.md rule 9).
//   2. The `createDetailPoller` interval lifecycle, driven with vitest
//      FAKE TIMERS over the real `setInterval`/`clearInterval`/`Date.now`
//      (which fake timers intercept). Pins: schedules a tick at the
//      cadence; stops itself on a terminal snapshot (NAV final + email,
//      and the NAV-rejected branch); honours the 5-minute cap; clears
//      on `stop()` (the component's destroy path) with no zombie tick;
//      idempotent `start()`; the in-flight re-entrancy guard; and that
//      a rejected refetch keeps the loop alive.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  createDetailPoller,
  isEmailSettled,
  isNavTerminal,
  isPollCapExceeded,
  isPollTerminal,
  POLL_CAP_MS,
  POLL_INTERVAL_MS,
  type DetailPollSnapshot,
} from "./invoice-detail-poll";

describe("isNavTerminal", () => {
  it("is true only for the two terminal pictogram states", () => {
    expect(isNavTerminal("Final")).toBe(true);
    expect(isNavTerminal("Rejected")).toBe(true);
    expect(isNavTerminal("Submitted")).toBe(false);
    expect(isNavTerminal("NotSubmitted")).toBe(false);
  });
});

describe("isEmailSettled", () => {
  it("is settled once an email-send entry exists, NAV still in flight", () => {
    expect(isEmailSettled(true, false)).toBe(true);
  });
  it("is NOT settled when no entry yet AND NAV still in flight", () => {
    expect(isEmailSettled(false, false)).toBe(false);
  });
  it("is settled via the opted-out shortcut: no entry but NAV terminal", () => {
    // The background tail emails BEFORE it submits, so a NAV-terminal
    // invoice with no email row means auto-send was toggled off —
    // nothing more will arrive, so we must not wait on it.
    expect(isEmailSettled(false, true)).toBe(true);
  });
  it("is settled when both signals are present", () => {
    expect(isEmailSettled(true, true)).toBe(true);
  });
});

describe("isPollTerminal", () => {
  const cases: Array<[DetailPollSnapshot, boolean]> = [
    [{ navState: "Final", hasEmailAttempt: true }, true],
    [{ navState: "Rejected", hasEmailAttempt: false }, true], // opted-out
    [{ navState: "Final", hasEmailAttempt: false }, true], // opted-out
    [{ navState: "Submitted", hasEmailAttempt: true }, false], // NAV not done
    [{ navState: "Submitted", hasEmailAttempt: false }, false],
    [{ navState: "NotSubmitted", hasEmailAttempt: true }, false],
    [{ navState: "NotSubmitted", hasEmailAttempt: false }, false],
  ];
  it.each(cases)("%o → %s", (snapshot, expected) => {
    expect(isPollTerminal(snapshot)).toBe(expected);
  });
});

describe("isPollCapExceeded", () => {
  it("is false strictly under the cap and true at/over it", () => {
    expect(isPollCapExceeded(POLL_CAP_MS - 1)).toBe(false);
    expect(isPollCapExceeded(POLL_CAP_MS)).toBe(true);
    expect(isPollCapExceeded(POLL_CAP_MS + 1)).toBe(true);
  });
});

const NON_TERMINAL: DetailPollSnapshot = {
  navState: "Submitted",
  hasEmailAttempt: false,
};
const TERMINAL_FINAL: DetailPollSnapshot = {
  navState: "Final",
  hasEmailAttempt: true,
};
const TERMINAL_REJECTED: DetailPollSnapshot = {
  navState: "Rejected",
  hasEmailAttempt: false,
};

/** Build a poller wired to the (faked) global timers + clock. */
function pollerWith(
  refetch: () => Promise<DetailPollSnapshot>,
  extra: {
    onCapExceeded?: () => void;
    onError?: (err: unknown) => void;
    intervalMs?: number;
    capMs?: number;
  } = {},
) {
  return createDetailPoller({
    refetch,
    now: () => Date.now(),
    setIntervalFn: (cb, ms) => setInterval(cb, ms),
    clearIntervalFn: (h) => clearInterval(h),
    ...extra,
  });
}

describe("createDetailPoller", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("schedules a refetch at the cadence and keeps running while non-terminal", async () => {
    const refetch = vi.fn().mockResolvedValue(NON_TERMINAL);
    const poller = pollerWith(refetch);
    poller.start();
    expect(poller.isRunning()).toBe(true);
    expect(refetch).toHaveBeenCalledTimes(0); // first tick is one interval out

    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(refetch).toHaveBeenCalledTimes(1);
    expect(poller.isRunning()).toBe(true);

    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(refetch).toHaveBeenCalledTimes(2);
    expect(poller.isRunning()).toBe(true);
  });

  it("stops itself when the snapshot is terminal (NAV final + email settled)", async () => {
    const refetch = vi.fn().mockResolvedValue(TERMINAL_FINAL);
    const poller = pollerWith(refetch);
    poller.start();

    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(refetch).toHaveBeenCalledTimes(1);
    expect(poller.isRunning()).toBe(false);

    // No further ticks after a terminal stop.
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS * 5);
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it("stops on the NAV-rejected terminal branch", async () => {
    const refetch = vi.fn().mockResolvedValue(TERMINAL_REJECTED);
    const poller = pollerWith(refetch);
    poller.start();
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(poller.isRunning()).toBe(false);
  });

  it("transitions live then stops: in-flight → terminal across ticks", async () => {
    const refetch = vi
      .fn<() => Promise<DetailPollSnapshot>>()
      .mockResolvedValueOnce(NON_TERMINAL) // still submitting
      .mockResolvedValueOnce(NON_TERMINAL)
      .mockResolvedValue(TERMINAL_FINAL); // NAV saved + emailed
    const poller = pollerWith(refetch);
    poller.start();

    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(poller.isRunning()).toBe(true);
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(poller.isRunning()).toBe(true);
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(refetch).toHaveBeenCalledTimes(3);
    expect(poller.isRunning()).toBe(false);
  });

  it("honours the cap: stops + warns if never terminal (injectable capMs)", async () => {
    const refetch = vi.fn().mockResolvedValue(NON_TERMINAL);
    const onCapExceeded = vi.fn();
    // Shrink the cap to 3 intervals so the test is fast and the cap
    // path (not the default constant) is what's exercised.
    const poller = pollerWith(refetch, {
      onCapExceeded,
      intervalMs: 2000,
      capMs: 6000,
    });
    poller.start();

    // Ticks at t=2000, 4000 fetch (under cap); the tick at t=6000 hits
    // the cap and stops BEFORE fetching, so only 2 fetches land.
    await vi.advanceTimersByTimeAsync(6000);

    expect(onCapExceeded).toHaveBeenCalledTimes(1);
    expect(poller.isRunning()).toBe(false);
    expect(refetch).toHaveBeenCalledTimes(2);

    // Cap-stopped: no zombie ticks afterwards.
    await vi.advanceTimersByTimeAsync(6000);
    expect(refetch).toHaveBeenCalledTimes(2);
  });

  it("the default cap is the documented 5 minutes", () => {
    expect(POLL_CAP_MS).toBe(5 * 60 * 1000);
  });

  it("stop() tears down the interval — no zombie tick (the destroy path)", async () => {
    const refetch = vi.fn().mockResolvedValue(NON_TERMINAL);
    const poller = pollerWith(refetch);
    poller.start();
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(refetch).toHaveBeenCalledTimes(1);

    poller.stop();
    expect(poller.isRunning()).toBe(false);
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS * 10);
    expect(refetch).toHaveBeenCalledTimes(1); // no further fetches
  });

  it("start() is idempotent — a second start does not stack a second interval", async () => {
    const refetch = vi.fn().mockResolvedValue(NON_TERMINAL);
    const poller = pollerWith(refetch);
    poller.start();
    poller.start();
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(refetch).toHaveBeenCalledTimes(1); // one interval, not two
  });

  it("skips overlapping ticks while a slow refetch is in flight", async () => {
    let resolveSlow: (s: DetailPollSnapshot) => void = () => {};
    const slow = new Promise<DetailPollSnapshot>((r) => {
      resolveSlow = r;
    });
    const refetch = vi
      .fn<() => Promise<DetailPollSnapshot>>()
      .mockReturnValueOnce(slow)
      .mockResolvedValue(NON_TERMINAL);
    const poller = pollerWith(refetch);
    poller.start();

    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS); // tick 1 starts the slow fetch
    expect(refetch).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS); // tick 2 skipped (in flight)
    expect(refetch).toHaveBeenCalledTimes(1);

    resolveSlow(NON_TERMINAL); // slow fetch resolves; in-flight clears
    await vi.advanceTimersByTimeAsync(0); // flush the resolution microtasks
    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS); // tick 3 fetches again
    expect(refetch).toHaveBeenCalledTimes(2);
    expect(poller.isRunning()).toBe(true);
  });

  it("keeps the loop alive when a refetch rejects (transient blip)", async () => {
    const onError = vi.fn();
    const refetch = vi
      .fn<() => Promise<DetailPollSnapshot>>()
      .mockRejectedValueOnce(new Error("network blip"))
      .mockResolvedValue(NON_TERMINAL);
    const poller = pollerWith(refetch, { onError });
    poller.start();

    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(onError).toHaveBeenCalledTimes(1);
    expect(poller.isRunning()).toBe(true); // survived the rejection

    await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    expect(refetch).toHaveBeenCalledTimes(2); // retried on the next tick
    expect(poller.isRunning()).toBe(true);
  });
});
