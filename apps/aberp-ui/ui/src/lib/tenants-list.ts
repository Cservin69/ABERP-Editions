// S433 — pure view-model for the Tenants admin screen. No DOM, no
// fetch: the per-row button-enable logic + display ordering live here so
// they're unit-testable without standing up Svelte or the Tauri invoke
// shim (CLAUDE.md rule 9 — tests verify the intent, not the chrome).
//
// The enable rules MIRROR the backend's registry invariants
// (`aberp::tenant_registry`): the SPA disables an action the backend
// would refuse so the operator never sees a dead-end ([[hulye-biztos]]),
// while the backend stays the source of truth ([[trust-code-not-operator]]).

import type { TenantRow } from "./api";

export interface TenantButtonState {
  /** Switch is offered for a bootable tenant (Active OR Demo) that isn't
   * already running. */
  canSwitch: boolean;
  /** Archive is refused for the running tenant, the only Active tenant
   * (must keep ≥1 Active), and the Demo tenant (the bundled safety net) —
   * mirrors `TenantRegistry::archive`. */
  canArchive: boolean;
  /** Restore is offered only for an Archived tenant. */
  canRestore: boolean;
}

/** Per-row action availability given the full list (the only-Active
 * guard needs the active count). */
export function buttonStateFor(row: TenantRow, rows: TenantRow[]): TenantButtonState {
  const activeCount = rows.filter((r) => r.state === "active").length;
  const bootable = row.state === "active" || row.state === "demo";
  return {
    canSwitch: bootable && !row.running,
    canArchive: row.state === "active" && !row.running && activeCount > 1,
    canRestore: row.state === "archived",
  };
}

/** S434 — the rows to show in the default view. When the operator has set
 * `hideDemo` AND a real (Active, non-demo) tenant exists, the bundled demo
 * is hidden from the list (it stays unarchivable — just out of the way).
 * The running tenant is NEVER hidden, even if it is the demo, so the
 * operator can always see + manage what they're currently in. */
export function visibleTenants(
  rows: TenantRow[],
  hideDemo: boolean,
  hasRealTenant: boolean,
): TenantRow[] {
  if (!hideDemo || !hasRealTenant) return rows;
  return rows.filter((r) => r.state !== "demo" || r.running);
}

/** Display order: the running tenant first, then Active, then Demo, then
 * Archived — each group alphabetised by slug. Stable + deterministic so
 * the list doesn't jump around between refreshes. */
export function orderTenants(rows: TenantRow[]): TenantRow[] {
  const rank = (r: TenantRow): number => {
    if (r.running) return 0;
    if (r.state === "active") return 1;
    if (r.state === "demo") return 2;
    return 3;
  };
  return [...rows].sort((a, b) => {
    const ra = rank(a);
    const rb = rank(b);
    if (ra !== rb) return ra - rb;
    return a.slug.localeCompare(b.slug);
  });
}
