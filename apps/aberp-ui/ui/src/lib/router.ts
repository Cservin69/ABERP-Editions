// PR-53 / session-73 — tiny hash-based router for the SPA's top-
// level navigation shell. Three menu items: Invoices, Tenant Settings,
// NAV Credentials. The router is intentionally minimal — a Map of
// route slugs to typed values plus a `current()` reader + a
// `subscribe()` callback. No dep, no path-params, no nested routes:
// the SPA has three flat screens and a single-segment hash is
// sufficient.
//
// An A-decision deferred: we considered pulling in a real Svelte
// router (e.g. svelte-spa-router) but the brief's guidance was
// explicit — "use a hash-based router that's small and zero-dep, OR
// roll a tiny one inline." This module is the tiny inline one
// (A185).

/** Typed slug union for the SPA's top-level routes. PR-53 surfaced
 * three; PR-54 / session-74 added `partners` (the saved-buyers
 * management screen + the typeahead's owner); PR-79 added
 * `maintenance` (the maintenance-area landing dashboard — the
 * destination of the topbar gear, with a tile grid of the area's
 * modules + live status counts); PR-86 / session-111 added
 * `invoices-new` (the full-page IssueInvoice form, promoted from a
 * modal-on-top-of-the-list to a routable surface so the operator can
 * deep-link / bookmark / browser-back the issuance flow and so the
 * surface itself can be as large as the operator's screen for a
 * legally-binding document review). Extending the union is a one-
 * line change here + a render arm in `App.svelte`. */
export type AppRoute =
  | "invoices"
  | "invoices-new"
  | "statistics"
  | "tenant"
  | "nav-credentials"
  | "partners"
  | "products"
  // S427 — quoting-machine master data (Master Data area, alongside
  // partners + products).
  | "machines"
  // S443 / ADR-0092 — QC inspection plans (Master Data area).
  | "inspection-plans"
  // S428 — customer-type margin profiles (Master Data area).
  | "margin-profiles"
  // S431 — Approved Vendor List (Master Data area).
  | "avl-vendors"
  | "work-orders"
  | "qa"
  | "dispatch"
  | "workshop"
  | "maintenance"
  | "restore-from-nav"
  | "adapters"
  | "material-catalogue"
  // S267 / PR-256 — quoting-engine tunables (engine internals; the
  // four routes hang off a dedicated `quoting` sub-nav under the
  // maintenance area, distinct from `material-catalogue` which keeps
  // its storefront-push posture under settings).
  | "quoting-complexity-rules"
  | "quoting-tolerance-multipliers"
  | "quoting-parameters"
  | "quoting-stock-adjustments"
  // S4 / ADR-0094 Gap 2 — machine-family rate catalogue.
  | "quoting-machine-rates"
  // S6 / ADR-0094 Gap 3 — gear-process coefficient catalogue.
  | "quoting-gear-processes"
  // T5 / ADR-0097 Part 2 — per-band tolerance cost-rate catalogue.
  | "quoting-tolerance-cost-rates"
  // S273 / PR-262 / ADR-0069 — material-side inventory balances. Lives
  // under the maintenance area's Quoting sub-nav alongside the
  // tunables (operators reach it when they need to bump on_hand_qty
  // after a material delivery).
  | "inventory-balances"
  // S281 / PR-266 — operator inspector for the storefront email-relay
  // queue (ADR-0007). Read-only list of `outbound_email_queue` rows
  // with state filters (queued/sending/sent/failed). Lives under the
  // maintenance area alongside the other operational queues.
  | "email-relay-queue"
  // S424 / session-424 — cross-domain audit-events screen. The general,
  // filterable view of the WHOLE ledger ("all operator activity, any
  // domain, paginated + filtered"). Operational area (a daily-useful
  // forensic tool — "what happened to quote X / what did I do today" in
  // one click, [[hulye-biztos]]), distinct from the per-invoice timeline.
  | "audit-events"
  // S426 / ADR-0082 — DB snapshot + restore operations screen. Operational
  // area; list of validated logical snapshots + snapshot-now + guarded
  // restore wizard (the 2026-06-11 ART corruption defence).
  | "snapshots"
  // S429 — read-only closed-loop calibration page. Per-family coefficient +
  // recent samples chart + recent skips. Computed, never operator-tuned
  // ([[trust-code-not-operator]]).
  | "calibration"
  // S432 — material-traceability chain-of-custody report. Operational
  // area; operator lookup by material id / heat lot (quotes, work orders,
  // invoices placeholder).
  | "material-traceability"
  // S439 — defense quality management. Operational area; NCR list with
  // in-page detail (transitions, linked CAPAs) + create + state transitions.
  | "quality-ncrs"
  // S440 — procurement. Operational area; PO list + create (AVL-gated) +
  // in-page detail + receiving (auto-NCR on failed inspection).
  | "purchase-orders"
  // S433 — multi-tenant admin (Settings area). List every tenant from
  // tenants.toml + add / switch (restart-based) / archive / restore.
  // Distinct from the singular `tenant` route, which is the running
  // tenant's seller-identity settings.
  | "tenants";

/** Default route the SPA falls back to on first paint (or on a hash
 * with an unknown slug). The Invoices list was the only screen
 * pre-PR-53, so it stays the default to match existing operator
 * muscle-memory. */
export const DEFAULT_ROUTE: AppRoute = "invoices";

/** Wire-form prefix the router emits on `location.hash`. The browser
 * back/forward stack records each route change as a new history
 * entry; deep-links from outside the app (e.g. a future docs link)
 * can target `#/tenant` and the SPA mounts directly into Tenant
 * Settings on first paint. */
export const HASH_PREFIX = "#/";

/** Parse a hash string (with or without the leading `#`) into an
 * `AppRoute`. Unknown slugs fall back to [`DEFAULT_ROUTE`] so an
 * operator-typed bogus URL fragment lands on the invoice list
 * instead of an empty pane. */
export function parseRoute(hash: string): AppRoute {
  // Strip the leading `#` and `/` if present so both `#/invoices`
  // and `invoices` parse equivalently. Defence-in-depth against
  // browsers that normalise the hash differently.
  let slug = hash;
  if (slug.startsWith("#")) slug = slug.slice(1);
  if (slug.startsWith("/")) slug = slug.slice(1);
  // Drop query strings and fragments-of-fragments (e.g. `?foo=bar`)
  // — the SPA doesn't use them today but a future widening shouldn't
  // confuse this layer.
  const qIdx = slug.indexOf("?");
  if (qIdx >= 0) slug = slug.slice(0, qIdx);
  switch (slug) {
    case "invoices":
      return "invoices";
    case "invoices-new":
      return "invoices-new";
    case "statistics":
      return "statistics";
    case "tenant":
      return "tenant";
    case "nav-credentials":
      return "nav-credentials";
    case "partners":
      return "partners";
    case "products":
      return "products";
    case "machines":
      return "machines";
    case "inspection-plans":
      return "inspection-plans";
    case "margin-profiles":
      return "margin-profiles";
    case "avl-vendors":
      return "avl-vendors";
    case "work-orders":
      return "work-orders";
    case "qa":
      return "qa";
    case "dispatch":
      return "dispatch";
    case "workshop":
      return "workshop";
    case "maintenance":
      return "maintenance";
    case "restore-from-nav":
      return "restore-from-nav";
    case "adapters":
      return "adapters";
    case "material-catalogue":
      return "material-catalogue";
    case "quoting-complexity-rules":
      return "quoting-complexity-rules";
    case "quoting-tolerance-multipliers":
      return "quoting-tolerance-multipliers";
    case "quoting-parameters":
      return "quoting-parameters";
    case "quoting-stock-adjustments":
      return "quoting-stock-adjustments";
    case "quoting-machine-rates":
      return "quoting-machine-rates";
    case "quoting-gear-processes":
      return "quoting-gear-processes";
    case "quoting-tolerance-cost-rates":
      return "quoting-tolerance-cost-rates";
    case "inventory-balances":
      return "inventory-balances";
    case "email-relay-queue":
      return "email-relay-queue";
    case "audit-events":
      return "audit-events";
    case "snapshots":
      return "snapshots";
    case "calibration":
      return "calibration";
    case "material-traceability":
      return "material-traceability";
    case "quality-ncrs":
      return "quality-ncrs";
    case "purchase-orders":
      return "purchase-orders";
    case "tenants":
      return "tenants";
    default:
      return DEFAULT_ROUTE;
  }
}

/** Compose the canonical `location.hash` form for a given route. */
export function routeHash(route: AppRoute): string {
  return `${HASH_PREFIX}${route}`;
}

/** Read the current route from `window.location.hash`. Falls back to
 * [`DEFAULT_ROUTE`] when running outside a browser (vitest) or when
 * the hash is empty / unknown. */
export function currentRoute(): AppRoute {
  if (typeof window === "undefined") return DEFAULT_ROUTE;
  return parseRoute(window.location.hash);
}

/** Subscribe to hash-change events. Returns an unsubscribe function
 * the caller invokes on component unmount. */
export function subscribeRoute(onChange: (route: AppRoute) => void): () => void {
  if (typeof window === "undefined") return () => {};
  const handler = () => onChange(parseRoute(window.location.hash));
  window.addEventListener("hashchange", handler);
  return () => window.removeEventListener("hashchange", handler);
}

/** Programmatically navigate to a route. Mutates `location.hash`,
 * which fires `hashchange` so subscribers re-render. */
export function navigateTo(route: AppRoute): void {
  if (typeof window === "undefined") return;
  window.location.hash = routeHash(route);
}
