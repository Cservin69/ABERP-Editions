// S282 / PR-267 — Empty-state classification for the Pricing tab.
//
// Extracted from `PricingJobsList.svelte` so the four-branch logic can
// be vitest-pinned without component-render tooling. The Svelte
// component renders directly off the returned `EmptyStateKind`.
//
// Honors [[trust-code-not-operator]]: the operator should never wonder
// why the pricing daemon appears silent. Each non-active state has a
// distinct copy + colour (RED venv-missing / AMBER spawn-errored /
// GREEN active-no-work / table-shown when rows.length > 0).

import type { PipelinePythonStatus } from "./api";

export type EmptyStateKind =
  /** rows.length > 0 — the table is rendered. */
  | "rows"
  /** rows.length === 0, daemon active, no pending submissions. GREEN. */
  | "active"
  /** rows.length === 0, resolver returned `not_resolved`. RED. */
  | "venv_missing"
  /** rows.length === 0, resolver succeeded but `daemon_spawned === false`
   *  (boot-block construction error). AMBER. */
  | "spawn_errored";

/** Pure four-branch classifier. The Svelte template renders off this. */
export function classifyEmptyState(
  rowCount: number,
  status: PipelinePythonStatus | null,
): EmptyStateKind {
  if (rowCount > 0) return "rows";
  if (status?.resolution_kind === "not_resolved") return "venv_missing";
  if (status && !status.daemon_spawned) return "spawn_errored";
  return "active";
}
