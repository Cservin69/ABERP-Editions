// PR-78 / session 101 — closed-vocab ERP module registry, organized
// by USAGE FREQUENCY into two AREAS (ADR-0041 §1):
//
//   - "operational" — the daily-driver workflow. Today: Invoicing.
//                     Future: Inventory, Accounting, Procurement.
//                     Front-and-center: the operator lives here all
//                     day.
//   - "maintenance" — master-data + settings, deliberately ONE
//                     LEVEL REMOVED from the operational nav so it
//                     does not clutter the day-to-day. Today: Master
//                     Data (Partners), Settings (Tenant, NAV
//                     credentials). Future: products, price lists,
//                     tax-rate templates. Accessed intentionally via
//                     the topbar's gear affordance, not co-listed
//                     with operational modules.
//
// This module is intentionally pure-data + a small set of lookup
// helpers. No DOM, no Svelte, no fetch. The chrome in App.svelte
// consumes `MODULES` + `modulesInArea` to render the area-scoped
// sidebar and uses `moduleForRoute` to derive the active area + the
// parent-module-of-the-active-route.
//
// Pinned by `erp-modules.test.ts`.

import type { AppRoute } from "./router";

/** Closed-vocab union of usage-frequency areas (ADR-0041 §1).
 * Two-tier separation: operational = daily driver, maintenance =
 * configuration + master data. The chrome shows ONE area at a time;
 * an explicit topbar affordance swaps between them. */
export type ErpArea = "operational" | "maintenance";

/** Closed-vocab union of every known ERP module id. Lifts to a Rust
 * `enum ErpModule` when the backend cut lands (PR-79+ per ADR-0041
 * §5); the string forms here MUST match the future Rust variant
 * snake/kebab names so backend route namespacing (`/api/<module-id>/...`)
 * mirrors the SPA chrome's grouping. CLAUDE.md rule 7 — deny-default:
 * a new module is an explicit one-line widening here, not a silent
 * fall-through. */
export type ErpModuleId = "invoicing" | "master-data" | "settings";

/** A route reference inside a module. `id` is the typed `AppRoute`
 * slug (the router's closed vocab); `label` is the chrome's display
 * string for the sidenav row. Today labels stay English to match
 * the existing flat sidebar (PR-53 / session-73).
 *
 * PR-87 / session-112 — `hidden` marks a sub-page that BELONGS to the
 * module (so `areaForRoute` resolves it and the area chrome stays
 * correct on deep-link) but is NOT rendered as a sidebar row. The
 * full-page IssueInvoice form (`invoices-new`) is reached via the
 * "+ New invoice" action on the list, not via a sidebar item — adding
 * a sidebar row would clutter the operational nav with an action the
 * operator already has one click away inside the daily-driver list.
 * Deep-link + browser-back still work because the route is registered
 * and the page chrome still mounts. */
export interface ErpRouteRef {
  id: AppRoute;
  label: string;
  hidden?: boolean;
}

/** A registered ERP module. See ADR-0041 §1 + §2 for the per-field
 * contract. `area` decides whether the module appears in the
 * operational sidebar or behind the maintenance gear. `glyph` is a
 * single printable Unicode mark; no icon-library dependency by
 * design (CLAUDE.md rule 2). */
export interface ErpModule {
  id: ErpModuleId;
  area: ErpArea;
  label_hu: string;
  label_en: string;
  glyph: string;
  routes: ErpRouteRef[];
}

/** Display title for each area, used by the chrome (sidebar
 * section caption, gear-button label, "back to ..." link text). */
export const AREA_LABELS: Record<ErpArea, { hu: string; en: string }> = {
  operational: { hu: "Munka", en: "Operational" },
  maintenance: { hu: "Karbantartás", en: "Maintenance" },
};

/** The registry. Order is the display order in the sidebar within
 * each area (top to bottom). Within operational: Invoicing only
 * today. Within maintenance: Master Data (referenced from invoicing)
 * before Settings (operator-rare-touch). Each module's `routes`
 * order is the display order within that module's sub-list.
 *
 * Adding a module: extend `ErpModuleId` above, add the entry here
 * with the chosen `area`. The route-coverage pin in
 * `erp-modules.test.ts` will fail loudly if a new `AppRoute` was
 * added without a registry home. */
export const MODULES: ErpModule[] = [
  {
    id: "invoicing",
    area: "operational",
    label_hu: "Számlázás",
    label_en: "Invoicing",
    glyph: "§",
    // PR-87 / session-112 — `invoices-new` is the full-page issuance
    // form (pre-PR-87 it was a `<dialog>` modal mounted inside
    // InvoiceList; PR-86 enlarged the modal which Ervin declined, this
    // PR finishes the container swap). It is REGISTERED under Invoicing
    // so `areaForRoute("invoices-new") === "operational"` and the
    // chrome stays in the operational area on deep-link / back-from-
    // form. It is MARKED `hidden: true` so the sidebar doesn't render a
    // second "New invoice" row beside "Invoices" — the action is
    // reached via the "+ New invoice" button on the list (one click,
    // contextual) and via deep link / browser back.
    routes: [
      { id: "invoices", label: "Invoices" },
      { id: "invoices-new", label: "New invoice", hidden: true },
    ],
  },
  {
    id: "master-data",
    area: "maintenance",
    label_hu: "Törzsadatok",
    label_en: "Master Data",
    glyph: "¶",
    routes: [
      { id: "partners", label: "Partners" },
      { id: "products", label: "Products" },
    ],
  },
  {
    id: "settings",
    area: "maintenance",
    label_hu: "Beállítások",
    label_en: "Settings",
    glyph: "◌",
    routes: [
      { id: "tenant", label: "Tenant settings" },
      { id: "nav-credentials", label: "NAV credentials" },
      // S180 / PR-180 — NAV-as-DR restore wizard. Maintenance-area
      // route under Settings (rare-touch, load-bearing-when-touched).
      { id: "restore-from-nav", label: "Restore from NAV" },
    ],
  },
];

/** Look up the module that owns a given route. Total over `AppRoute`
 * by construction — the coverage pin enforces this. Returns the
 * matched `ErpModule` for in-chrome rendering of "this route's
 * parent module" and (transitively) the active area.
 *
 * Returns `null` ONLY if the registry has been edited inconsistently
 * (a route exists in `AppRoute` but no module claims it). The pin
 * catches that at build time, so callers in production code do not
 * need to handle the null path — but the type is honest about the
 * possibility rather than throwing, so a future hand-edited registry
 * bug surfaces as a missing-parent-header in chrome (visible) rather
 * than a runtime exception (silent crash). */
export function moduleForRoute(route: AppRoute): ErpModule | null {
  for (const m of MODULES) {
    for (const r of m.routes) {
      if (r.id === route) return m;
    }
  }
  return null;
}

/** Derive the active area for the route the operator is currently
 * on. The chrome uses this to (a) decide which area's modules to
 * render in the sidebar and (b) decide which area the topbar's
 * area-swap button targets.
 *
 * Resolution order:
 *   1. A module owns the route → that module's area.
 *   2. The route is an area-landing (`AREA_LANDING_ROUTES`) →
 *      that landing's area (PR-79 — the maintenance dashboard's
 *      home route).
 *   3. Defence-in-depth fallback "operational" for unknown routes
 *      (`parseRoute` already routes unknowns to the default
 *      `invoices` route, so this branch is rarely hit). */
export function areaForRoute(route: AppRoute): ErpArea {
  const owner = moduleForRoute(route);
  if (owner !== null) return owner.area;
  for (const [area, landing] of Object.entries(AREA_LANDING_ROUTES) as [
    ErpArea,
    AppRoute,
  ][]) {
    if (landing === route) return area;
  }
  return "operational";
}

/** Return every module belonging to a given area, preserving the
 * registry's declared order. Used by the sidebar to render the
 * active area's contents only. */
export function modulesInArea(area: ErpArea): ErpModule[] {
  return MODULES.filter((m) => m.area === area);
}

/** PR-79 / session 102 — per-area landing route. The chrome's
 * area-swap (topbar gear) navigates to the landing for the target
 * area; the landing is the area's "home". Today:
 *
 *   - operational → no landing route; the area's daily-driver
 *     screen (Invoices) IS the home, so the entry point falls
 *     through to the first module's first route.
 *   - maintenance → "maintenance" — a tile-grid dashboard that
 *     glances at each maintenance module + live status counts
 *     (partner count, bank-account count, NAV cred presence).
 *     PR-79 ships this dashboard.
 *
 * The closed-vocab `AREA_LANDING_ROUTES` table is the single source
 * of truth: the route-coverage pin in `erp-modules.test.ts` exempts
 * these from per-module ownership (they are AREA affordances, not
 * MODULE routes), and `defaultRouteForArea` consults this table
 * first before falling through to the first-module-first-route
 * default. Adding a future operational dashboard is a one-line
 * widening here. */
export const AREA_LANDING_ROUTES: Partial<Record<ErpArea, AppRoute>> = {
  maintenance: "maintenance",
};

/** PR-78 / PR-79 — the route the chrome's area-swap (topbar gear)
 * navigates to when entering an area. PR-79 elevates the maintenance
 * area to its own landing dashboard route (`#/maintenance`); the
 * operational area keeps the pre-existing fall-through to the first
 * module's first VISIBLE route (`#/invoices`) because that is the
 * area's actual daily-driver home, not a dashboard.
 *
 * PR-87 / session-112 — skip routes marked `hidden: true` (e.g. the
 * full-page `invoices-new` issuance form) so the topbar's "back to
 * operational" button never lands the operator on an action sub-page
 * by accident. A module with ONLY hidden routes effectively has no
 * area entry; in practice every module ships at least one visible
 * row, so this is defence-in-depth.
 *
 * Returns `null` only for an empty area with no landing — a registry
 * inconsistency the pin would catch. */
export function defaultRouteForArea(area: ErpArea): AppRoute | null {
  const landing = AREA_LANDING_ROUTES[area];
  if (landing !== undefined) return landing;
  const modules = modulesInArea(area);
  if (modules.length === 0) return null;
  const firstVisible = modules[0].routes.find((r) => r.hidden !== true);
  return firstVisible?.id ?? null;
}

// ── PR-79 / session 102 — maintenance dashboard tile config ────────────
//
// The maintenance landing dashboard (`#/maintenance`) renders a tile
// grid: one tile per *route* in the maintenance area. Each tile shows
// a bilingual label + description plus a live "status" — a small
// glance metric fetched from an existing read-only backend route. The
// statusKind is a closed-vocab discriminator the dashboard component
// dispatches on; adding a new status-kind is a deliberate one-line
// widening here + a render arm in the component (CLAUDE.md rule 7,
// surface conflicts don't average them).
//
// Pinned by `erp-modules.test.ts`:
//   - tile shape (non-empty fields, bilingual labels + descriptions),
//   - one tile per non-landing maintenance route,
//   - every tile's moduleId resolves to a maintenance-area module,
//   - every tile's route is a maintenance-area route owned by its
//     declared moduleId,
//   - the statusKind set matches the closed vocab.

/** Closed-vocab status discriminator on a maintenance tile. The
 * dashboard component dispatches on this to pick the read endpoint +
 * the chip's render. Adding a status kind is an explicit widening
 * (deny-default) — there is no "Other" or "Unknown" bucket. */
export type MaintenanceTileStatusKind =
  | "PartnerCount"
  | "ProductCount"
  | "BankAccountCount"
  | "NavCredStatus"
  // S180 / PR-180 — count of already-restored invoices in the
  // `restored_invoice` mirror table. The tile's chip surfaces "N
  // restored rows" so the operator can see at a glance whether
  // disaster recovery has been used.
  | "RestoredInvoiceCount";

/** One tile on the maintenance landing dashboard. The dashboard
 * renders the tiles grouped under their sub-area headers (today:
 * MASTER DATA, SETTINGS) — i.e. by the resolved module's id. The
 * tile knows nothing about fetching; the dashboard component owns
 * the read calls (failure-isolated per tile per the PR-74/PR-75
 * loadError + retry pattern). */
export interface MaintenanceTile {
  moduleId: ErpModuleId;
  route: AppRoute;
  label_hu: string;
  label_en: string;
  description_hu: string;
  description_en: string;
  statusKind: MaintenanceTileStatusKind;
}

/** The maintenance landing's tile registry. Display order is the
 * order here. One tile per non-landing maintenance route — adding a
 * new maintenance route without a tile fails the coverage pin in
 * `erp-modules.test.ts`, surfacing as a build error rather than a
 * silently missing tile on the dashboard. */
export const MAINTENANCE_TILES: MaintenanceTile[] = [
  {
    moduleId: "master-data",
    route: "partners",
    label_hu: "Partnerek",
    label_en: "Partners",
    description_hu: "Ügyfelek és beszállítók kezelése",
    description_en: "Manage customers & vendors",
    statusKind: "PartnerCount",
  },
  {
    moduleId: "master-data",
    route: "products",
    label_hu: "Termékek",
    label_en: "Products",
    description_hu: "Katalógus: név, mértékegység, ár",
    description_en: "Catalog: name, unit of measure, price",
    statusKind: "ProductCount",
  },
  {
    moduleId: "settings",
    route: "tenant",
    label_hu: "Cégadatok",
    label_en: "Tenant Settings",
    description_hu: "Azonosság, bankszámlák, megjelenés",
    description_en: "Identity, bank accounts, branding",
    statusKind: "BankAccountCount",
  },
  {
    moduleId: "settings",
    route: "nav-credentials",
    label_hu: "NAV hitelesítés",
    label_en: "NAV Credentials",
    description_hu: "Technikai felhasználó és kulcsok",
    description_en: "Technical user & keys",
    statusKind: "NavCredStatus",
  },
  // S180 / PR-180 — NAV-as-DR restore wizard tile. Operator-touch
  // surface for "the local DuckDB is gone — pull our year-of-record
  // from NAV." Rare-touch, load-bearing-when-touched.
  {
    moduleId: "settings",
    route: "restore-from-nav",
    label_hu: "Visszaállítás NAV-ból",
    label_en: "Restore from NAV",
    description_hu: "Vészhelyzeti adat-visszaállítás",
    description_en: "Disaster recovery — restore invoice data",
    statusKind: "RestoredInvoiceCount",
  },
];
