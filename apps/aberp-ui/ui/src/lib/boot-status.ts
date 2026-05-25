// PR-45a / session-61 — pure-module helpers for the SPA's boot-state
// rendering decision. The SPA's App.svelte polls `getBootStatus()`
// and maps the response into one of three view-modes via
// `bootViewMode`; a Failed status additionally surfaces a small
// hint list (`failureHints`) that points the operator at the
// likeliest common causes named in the brief.
//
// The helpers are split out from the Svelte component so vitest can
// pin them without mounting the component (component-test runner is
// named-deferred per CLAUDE.md rule 2; the composer-pin pattern is
// the precedent for every per-state UI dispatch the shell needs —
// see A156, A161, A163).

import type { BootStatus, BootStatusResponse } from "./api";

/** What the SPA renders for a given boot lifecycle state.
 *
 * PR-46α / session-62 added the `setup` view-mode for the first-run
 * NAV-credentials wizard. PR-51 / session-71 added the
 * `seller-config` view-mode for the seller-identity wizard (chained
 * after NAV creds when `~/.aberp/<tenant>/seller.toml` is missing).
 * The mapping is total over `BootStatus`; no default arm so a future
 * variant added without a matching map row fails `npm run check`. */
export type BootViewMode =
  | "loading"
  | "setup"
  | "seller-config"
  | "ready"
  | "error";

/** Map a boot-status string to a view mode. Total over the typed
 * union; no default arm so a future variant added to `BootStatus`
 * without a matching map row fails `npm run check`. */
export function bootViewMode(status: BootStatus): BootViewMode {
  switch (status) {
    case "starting":
      return "loading";
    case "needs-setup":
      return "setup";
    case "needs-seller-config":
      return "seller-config";
    case "ready":
      return "ready";
    case "failed":
      return "error";
  }
}

/** Extract the operator-visible error message from a boot snapshot.
 * Returns the verbatim `error` field on a Failed snapshot;
 * otherwise `null`. Per CLAUDE.md rule 12, an explicit `null` is
 * better than an empty string so the SPA caller can decide whether
 * to render an inline pane. PR-46α / session-62: `"needs-setup"`
 * is NOT a failure state — it's an operator-actionable first-run
 * step — so it also returns `null` here. */
export function bootErrorMessage(snapshot: BootStatusResponse): string | null {
  if (snapshot.status !== "failed") {
    return null;
  }
  return snapshot.error ?? "backend boot failed with no error message";
}

/** Common-cause hints surfaced under the error pane per the brief.
 * The list is deliberately short — three causes that account for
 * almost every failure mode an operator can hit on a fresh
 * workstation. Order is most-likely first. */
export const FAILURE_HINTS: readonly string[] = [
  "NAV credentials missing — run `aberp setup-nav-credentials --tenant <id>` once.",
  "Database file is locked — another `aberp` process may still be running.",
  "Loopback port is unavailable — check for a stale `aberp serve` process.",
];

/** Latest line from the recent-logs ring buffer — the SPA's loading
 * pane shows this prominently so the operator sees forward motion
 * during the cold-boot window. Returns `null` when the buffer is
 * empty (which is the case at the very first poll, before the
 * backend has emitted any stderr line). */
export function latestLogLine(snapshot: BootStatusResponse): string | null {
  if (snapshot.recent_logs.length === 0) {
    return null;
  }
  return snapshot.recent_logs[snapshot.recent_logs.length - 1] ?? null;
}
