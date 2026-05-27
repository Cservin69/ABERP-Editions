// PR-78 / session 101 — vitest pin for the closed-vocab ERP module
// registry + the two-area usage-frequency split. Invariants per
// ADR-0041 §7:
//
//   1. Registry shape — every entry carries non-empty id /
//      label_hu / label_en / glyph / routes; module ids unique;
//      route ids unique across the registry; area is a closed-vocab
//      value.
//   2. Total route coverage — every value of `AppRoute` appears in
//      exactly one module's `routes` list. Adding an `AppRoute`
//      variant without a registry home fails here loudly.
//   3. `moduleForRoute` lookups — every existing route returns its
//      typed owning module.
//   4. Area split — each route's area matches the ADR-0041 §2
//      table; `areaForRoute` agrees with `moduleForRoute(route)?.area`.
//   5. Area helpers — `modulesInArea` preserves order and partitions
//      the registry; `defaultRouteForArea` returns the first route
//      of the first module in that area.
//
// These are the *load-bearing* invariants the chrome consumes; the
// rest of the registry (label text, glyph character) is data that
// can evolve without breaking the chrome.

import { describe, expect, it } from "vitest";

import {
  AREA_LABELS,
  AREA_LANDING_ROUTES,
  MAINTENANCE_TILES,
  MODULES,
  areaForRoute,
  defaultRouteForArea,
  modulesInArea,
  moduleForRoute,
  type ErpArea,
  type ErpModuleId,
  type MaintenanceTileStatusKind,
} from "./erp-modules";
import type { AppRoute } from "./router";

// Every value of `AppRoute` must be enumerated here exactly once.
// This array IS the test source-of-truth — a new AppRoute variant
// without a corresponding entry here causes a TS narrowing failure
// in the typed `EXPECTED_OWNER` / `EXPECTED_AREA` records below, so
// the pin can never silently drift away from the router's actual
// closed vocab.
const ALL_APP_ROUTES: AppRoute[] = [
  "invoices",
  "invoices-new",
  "partners",
  "tenant",
  "nav-credentials",
  "maintenance",
];

// PR-79 / session 102 — closed set of AREA-landing routes. These are
// chrome affordances (the maintenance area's home dashboard), not
// module routes — they belong to NO module's `routes` list by
// design. The coverage pin below exempts these from per-module
// ownership so a future operational landing dashboard can join the
// same pattern with a single registry edit.
const AREA_LANDING_ROUTE_SET: Set<AppRoute> = new Set<AppRoute>([
  "maintenance",
]);

// The expected module-id for each non-landing AppRoute. Hand-pinned
// so the grouping is verified against the ADR §2 table, not against
// the registry's own self-consistent restatement of it. If a future
// PR regroups a route, this table changes alongside the registry —
// and the diff makes the regrouping visible at PR review time.
// Landing routes (`maintenance`) are intentionally absent.
const EXPECTED_OWNER: Partial<Record<AppRoute, ErpModuleId>> = {
  invoices: "invoicing",
  "invoices-new": "invoicing",
  partners: "master-data",
  tenant: "settings",
  "nav-credentials": "settings",
};

// The expected area for each AppRoute. The two-area usage-frequency
// split: operational holds the daily workflow; maintenance holds
// the configuration + master-data routes one level removed. The
// `maintenance` landing route itself lives in the maintenance area
// (it IS the area's home).
const EXPECTED_AREA: Record<AppRoute, ErpArea> = {
  invoices: "operational",
  "invoices-new": "operational",
  partners: "maintenance",
  tenant: "maintenance",
  "nav-credentials": "maintenance",
  maintenance: "maintenance",
};

// Closed-vocab set of accepted status kinds on a maintenance tile.
// Sourced from the union in `erp-modules.ts`. If a future tile adds a
// new status-kind the dashboard renders, the union widens by one and
// this set widens alongside — there is no "Other" bucket.
const ALL_TILE_STATUS_KINDS: Set<MaintenanceTileStatusKind> = new Set<
  MaintenanceTileStatusKind
>(["PartnerCount", "BankAccountCount", "NavCredStatus"]);

// Every area must have a stable bilingual label and at least one
// module — the chrome's topbar gear/back affordance assumes both.
const ALL_AREAS: ErpArea[] = ["operational", "maintenance"];

describe("MODULES registry shape", () => {
  it("every module carries non-empty id, labels, glyph, routes, area", () => {
    for (const m of MODULES) {
      expect(m.id.length).toBeGreaterThan(0);
      expect(m.label_hu.trim().length).toBeGreaterThan(0);
      expect(m.label_en.trim().length).toBeGreaterThan(0);
      expect(m.glyph.length).toBeGreaterThan(0);
      expect(m.routes.length).toBeGreaterThan(0);
      // Closed-vocab assertion: every module's area is one of the
      // known ErpArea values. Catches a typo at registry-write time.
      expect(ALL_AREAS).toContain(m.area);
      for (const r of m.routes) {
        expect(r.id.length).toBeGreaterThan(0);
        expect(r.label.trim().length).toBeGreaterThan(0);
      }
    }
  });

  it("module ids are unique", () => {
    const ids = MODULES.map((m) => m.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("route ids are unique across the entire registry", () => {
    // A route can only belong to ONE module (ADR-0041 §1 identity
    // invariant). Catches a paste-error that double-claims a route
    // when adding a new module.
    const allRoutes: string[] = [];
    for (const m of MODULES) {
      for (const r of m.routes) allRoutes.push(r.id);
    }
    expect(new Set(allRoutes).size).toBe(allRoutes.length);
  });
});

describe("total route coverage", () => {
  it("every non-landing AppRoute is claimed by exactly one module", () => {
    // ADR-0041 §7 + §8: deny-default. A new AppRoute variant added
    // to router.ts without a registry home (and without being
    // promoted to an area-landing) fails here naming the orphan, so
    // a future contributor can't silently sweep a new route into a
    // "misc" bucket (there is no misc bucket). Landing routes (PR-79
    // — `maintenance`) are deliberately exempt: they are CHROME
    // affordances, not module-owned screens.
    for (const route of ALL_APP_ROUTES) {
      if (AREA_LANDING_ROUTE_SET.has(route)) continue;
      const claimants = MODULES.filter((m) =>
        m.routes.some((r) => r.id === route),
      );
      expect(
        claimants.length,
        `route "${route}" should be claimed by exactly one module`,
      ).toBe(1);
    }
  });

  it("landing routes are claimed by NO module", () => {
    // The mirror of the pin above: an area-landing route MUST NOT
    // appear in any module's `routes` list. A future regression that
    // claims the landing under a module would conflict with the
    // chrome's "this route IS the area's home" semantics.
    for (const route of AREA_LANDING_ROUTE_SET) {
      const claimants = MODULES.filter((m) =>
        m.routes.some((r) => r.id === route),
      );
      expect(
        claimants.length,
        `landing route "${route}" must not be claimed by any module`,
      ).toBe(0);
    }
  });

  it("the grouping matches ADR-0041 §2 verbatim", () => {
    // Hand-pinned table. Catches a regrouping that the registry
    // alone wouldn't surface (e.g. moving `partners` to `settings`
    // would pass the totality pin above but break this one).
    for (const route of ALL_APP_ROUTES) {
      if (AREA_LANDING_ROUTE_SET.has(route)) continue;
      const owner = MODULES.find((m) =>
        m.routes.some((r) => r.id === route),
      );
      expect(owner?.id).toBe(EXPECTED_OWNER[route]);
    }
  });
});

describe("moduleForRoute lookup", () => {
  it("returns the owning module for every non-landing AppRoute", () => {
    for (const route of ALL_APP_ROUTES) {
      if (AREA_LANDING_ROUTE_SET.has(route)) continue;
      const m = moduleForRoute(route);
      expect(m).not.toBeNull();
      expect(m?.id).toBe(EXPECTED_OWNER[route]);
    }
  });

  it("returns null for an area-landing route (no module owns it)", () => {
    // The maintenance landing is a chrome affordance, not a module
    // route. `moduleForRoute` honestly returns `null` rather than
    // throwing or fabricating an owner — chrome consumers branch on
    // null to render the area-landing pane instead of a module's
    // route pane.
    for (const route of AREA_LANDING_ROUTE_SET) {
      expect(moduleForRoute(route)).toBeNull();
    }
  });

  it("returned module's routes list actually contains the queried route", () => {
    // Defence-in-depth: moduleForRoute could in principle return a
    // module by accident (e.g. an off-by-one in a future refactor).
    // Pin that the returned module's routes ACTUALLY includes the
    // route we asked about.
    for (const route of ALL_APP_ROUTES) {
      if (AREA_LANDING_ROUTE_SET.has(route)) continue;
      const m = moduleForRoute(route);
      expect(m?.routes.some((r) => r.id === route)).toBe(true);
    }
  });
});

describe("area split (operational vs maintenance)", () => {
  it("each AppRoute lives in the expected area", () => {
    for (const route of ALL_APP_ROUTES) {
      expect(areaForRoute(route)).toBe(EXPECTED_AREA[route]);
    }
  });

  it("areaForRoute agrees with moduleForRoute(route)?.area for module routes", () => {
    // Landing routes have no owning module by design, so
    // `moduleForRoute(route)?.area` is undefined for them — this
    // mirror pin only applies to module-owned routes.
    for (const route of ALL_APP_ROUTES) {
      if (AREA_LANDING_ROUTE_SET.has(route)) continue;
      expect(areaForRoute(route)).toBe(moduleForRoute(route)?.area);
    }
  });

  it("AREA_LABELS has a non-empty HU + EN label for every area", () => {
    for (const a of ALL_AREAS) {
      const label = AREA_LABELS[a];
      expect(label.hu.trim().length).toBeGreaterThan(0);
      expect(label.en.trim().length).toBeGreaterThan(0);
    }
  });
});

describe("modulesInArea + defaultRouteForArea", () => {
  it("modulesInArea preserves registry order within each area", () => {
    const op = modulesInArea("operational");
    const mt = modulesInArea("maintenance");
    expect(op.map((m) => m.id)).toEqual(["invoicing"]);
    expect(mt.map((m) => m.id)).toEqual(["master-data", "settings"]);
  });

  it("modulesInArea partitions the registry (union covers every module, no overlap)", () => {
    const union = [
      ...modulesInArea("operational"),
      ...modulesInArea("maintenance"),
    ];
    expect(union.length).toBe(MODULES.length);
    expect(new Set(union.map((m) => m.id)).size).toBe(MODULES.length);
  });

  it("defaultRouteForArea returns the area landing first, else the first route of the first module", () => {
    // PR-79 / session 102 — the maintenance area gained a dedicated
    // landing dashboard route (`maintenance`). The topbar gear
    // navigates the operator there rather than jumping past the
    // landing into the first module's first route — the landing IS
    // the area's home.
    //
    // The operational area has no landing dashboard (Tier-3 pushback:
    // the daily-driver Invoice list IS the home), so it still falls
    // through to the first-module-first-route default (`invoices`).
    expect(defaultRouteForArea("operational")).toBe("invoices");
    expect(defaultRouteForArea("maintenance")).toBe("maintenance");
  });
});

describe("maintenance dashboard tiles (PR-79)", () => {
  it("every tile carries non-empty bilingual labels + descriptions + a valid statusKind", () => {
    for (const tile of MAINTENANCE_TILES) {
      expect(tile.label_hu.trim().length).toBeGreaterThan(0);
      expect(tile.label_en.trim().length).toBeGreaterThan(0);
      expect(tile.description_hu.trim().length).toBeGreaterThan(0);
      expect(tile.description_en.trim().length).toBeGreaterThan(0);
      expect(ALL_TILE_STATUS_KINDS.has(tile.statusKind)).toBe(true);
    }
  });

  it("every tile's moduleId resolves to a maintenance-area module", () => {
    // A tile pointing at an operational-area module would surface
    // an operational route on the maintenance dashboard, breaking
    // the area split. Pin this loud — the maintenance dashboard
    // belongs to the maintenance area only (ADR-0041 §2).
    for (const tile of MAINTENANCE_TILES) {
      const mod = MODULES.find((m) => m.id === tile.moduleId);
      expect(mod, `tile.moduleId "${tile.moduleId}" must exist in MODULES`).not.toBe(
        undefined,
      );
      expect(mod?.area).toBe("maintenance");
    }
  });

  it("every tile's route is owned by the tile's declared moduleId", () => {
    // Defence-in-depth: catches a paste-error tile that names module
    // X but a route owned by module Y (would surface a wrong sub-
    // area header on the dashboard).
    for (const tile of MAINTENANCE_TILES) {
      const owner = moduleForRoute(tile.route);
      expect(owner?.id).toBe(tile.moduleId);
    }
  });

  it("exactly one tile per non-landing maintenance route", () => {
    // PR-79 dashboard contract: every operator-visible maintenance
    // route has exactly one glanceable tile on the landing, no more
    // (duplicate tile clutters the grid) and no less (an orphan
    // route is reachable only via the sidebar, breaking the
    // dashboard's "this area at a glance" promise).
    const maintenanceRoutes: AppRoute[] = [];
    for (const m of modulesInArea("maintenance")) {
      for (const r of m.routes) maintenanceRoutes.push(r.id);
    }
    expect(maintenanceRoutes.length).toBeGreaterThan(0);
    for (const route of maintenanceRoutes) {
      const tiles = MAINTENANCE_TILES.filter((t) => t.route === route);
      expect(
        tiles.length,
        `maintenance route "${route}" should have exactly one tile`,
      ).toBe(1);
    }
    // Mirror: no tile points at a NON-maintenance route.
    const maintenanceRouteSet = new Set(maintenanceRoutes);
    for (const tile of MAINTENANCE_TILES) {
      expect(maintenanceRouteSet.has(tile.route)).toBe(true);
    }
    // Mirror: tile count exactly matches the route count.
    expect(MAINTENANCE_TILES.length).toBe(maintenanceRoutes.length);
  });

  it("every maintenance module is represented by at least one tile", () => {
    // The sub-area headers on the dashboard (MASTER DATA, SETTINGS)
    // come from the set of moduleIds touched by the tile list. A
    // module with zero tiles would have NO header rendered — the
    // operator would never see its name on the dashboard despite it
    // being a real area resident. Pin every maintenance module gets
    // at least one tile.
    const tileModules = new Set(MAINTENANCE_TILES.map((t) => t.moduleId));
    for (const mod of modulesInArea("maintenance")) {
      expect(
        tileModules.has(mod.id),
        `maintenance module "${mod.id}" must have ≥1 dashboard tile`,
      ).toBe(true);
    }
  });

  it("tile routes are unique (no duplicate destinations on the dashboard)", () => {
    const ids = MAINTENANCE_TILES.map((t) => t.route);
    expect(new Set(ids).size).toBe(ids.length);
  });
});

describe("AREA_LANDING_ROUTES (PR-79)", () => {
  it("maintenance lands at #/maintenance; operational has no dedicated landing", () => {
    // The topbar gear (`⚙ MAINTENANCE`) navigates to
    // AREA_LANDING_ROUTES.maintenance, NOT to the first maintenance
    // module's first route. This is the visible behavior change
    // from PR-78.
    expect(AREA_LANDING_ROUTES.maintenance).toBe("maintenance");
    // Operational stays bare — the Invoices list IS the home; no
    // dashboard widget set per the roadmap Tier-3 pushback.
    expect(AREA_LANDING_ROUTES.operational).toBeUndefined();
  });

  it("the maintenance landing route is itself in the maintenance area", () => {
    expect(areaForRoute("maintenance")).toBe("maintenance");
  });
});

describe("area-swap round-trip (PR-81)", () => {
  // PR-81 — pin for the topbar area-swap affordance the chrome wires
  // in App.svelte (`swapArea()`):
  //
  //   target = activeArea === "operational" ? "maintenance" : "operational";
  //   dest   = defaultRouteForArea(target);
  //   navigateTo(dest);
  //
  // The pre-PR-81 regression: clicking `← OPERATIONAL` from the
  // maintenance dashboard did not navigate back to `#/invoices`.
  // The unit-level pins on `defaultRouteForArea` + `areaForRoute`
  // each pass independently, but the COMPOSED round-trip is what the
  // operator actually experiences. These pins guard the composition
  // from EITHER direction so a future regression that broke ONE
  // direction (e.g. a re-grouping that moved the operational landing
  // or a typo that re-pointed AREA_LANDING_ROUTES) fails here.

  it("from every operational-area route, swap target lands in the maintenance area", () => {
    for (const route of ALL_APP_ROUTES) {
      if (areaForRoute(route) !== "operational") continue;
      // Mirror swapArea() in App.svelte.
      const dest = defaultRouteForArea("maintenance");
      expect(dest, `swap from "${route}" must yield a non-null dest`).not.toBeNull();
      expect(
        areaForRoute(dest as AppRoute),
        `swap from "${route}" should land in the maintenance area`,
      ).toBe("maintenance");
    }
  });

  it("from every maintenance-area route, swap target lands in the operational area", () => {
    // The pre-PR-81 broken direction. From maintenance — landing or
    // any module route — the swap MUST resolve to an operational-area
    // route, NOT stay in maintenance. A regression that left the
    // operator on the maintenance dashboard would fail here loudly.
    for (const route of ALL_APP_ROUTES) {
      if (areaForRoute(route) !== "maintenance") continue;
      const dest = defaultRouteForArea("operational");
      expect(dest, `swap from "${route}" must yield a non-null dest`).not.toBeNull();
      expect(
        areaForRoute(dest as AppRoute),
        `swap from "${route}" should land in the operational area`,
      ).toBe("operational");
    }
  });

  it("swap landings are the documented routes (operational→invoices, maintenance→maintenance)", () => {
    // The two area landings the chrome actually navigates to. Hand-
    // pinned so a future PR that swapped either landing surfaces the
    // change at this test, not silently on the operator's screen.
    expect(defaultRouteForArea("operational")).toBe("invoices");
    expect(defaultRouteForArea("maintenance")).toBe("maintenance");
  });

  it("deep-link into the maintenance landing route resolves to the maintenance area", () => {
    // Deep-linking `#/maintenance` MUST surface the maintenance chrome
    // (sidebar = maintenance modules, topbar button = `← OPERATIONAL`).
    // Pre-PR-79 this route did not exist; pre-PR-81 the chrome's swap
    // logic mis-handled the round-trip out of it. Pin both that the
    // route is registered AND that it lives in the right area.
    expect(areaForRoute("maintenance")).toBe("maintenance");
    // And the swap-out direction matches what the topbar button
    // promises ("← OPERATIONAL" → invoices).
    expect(defaultRouteForArea("operational")).toBe("invoices");
  });
});

describe("invoices-new sub-page (PR-87)", () => {
  // PR-87 / session-112 — the Issue Invoice form is now a full-page
  // route (`#/invoices-new`), not a modal. The route is REGISTERED
  // under the invoicing module so area-routing stays correct on
  // deep-link, but MARKED `hidden: true` so the operational sidebar
  // does not gain a second "New invoice" row beside "Invoices". The
  // entry point is the contextual "+ New invoice" action on the
  // invoices list; deep-link + browser-back still resolve the chrome.

  it("'invoices-new' resolves to the invoicing module in the operational area", () => {
    // The chrome's area derivation reads moduleForRoute first; a
    // regression that detached the sub-page from invoicing would
    // surface here (the topbar would flip to `← OPERATIONAL` on this
    // route, which would be a chrome bug because the operator IS in
    // the operational area).
    const owner = moduleForRoute("invoices-new");
    expect(owner?.id).toBe("invoicing");
    expect(owner?.area).toBe("operational");
    expect(areaForRoute("invoices-new")).toBe("operational");
  });

  it("'invoices-new' is marked hidden so the sidebar does not render it as a row", () => {
    // The sidebar filters routes by `r.hidden !== true` (App.svelte).
    // Pre-PR-87 a registry entry with no hidden flag would have
    // appeared as "New invoice" beside "Invoices" — cluttering the
    // operational nav with an action already reachable via the list's
    // "+ New invoice" button. Pin the flag is set so the chrome stays
    // clean.
    const invoicing = MODULES.find((m) => m.id === "invoicing");
    const ref = invoicing?.routes.find((r) => r.id === "invoices-new");
    expect(ref?.hidden).toBe(true);
  });

  it("'invoices' (the daily-driver list) is NOT hidden", () => {
    // Mirror of the pin above: the muscle-memory home stays visible
    // in the sidebar. A regression that mistakenly flipped the wrong
    // route's hidden flag would surface here.
    const invoicing = MODULES.find((m) => m.id === "invoicing");
    const ref = invoicing?.routes.find((r) => r.id === "invoices");
    expect(ref?.hidden).toBeFalsy();
  });

  it("'invoices-new' is NOT the area's default landing (the daily list still is)", () => {
    // The topbar's `← OPERATIONAL` button must keep landing on the
    // invoices list, NOT on the issuance form. A regression that
    // re-ordered Invoicing's routes (putting `invoices-new` first)
    // would silently flip the area landing — pin against that.
    expect(defaultRouteForArea("operational")).toBe("invoices");
  });
});
