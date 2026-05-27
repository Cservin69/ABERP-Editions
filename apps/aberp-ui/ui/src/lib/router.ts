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
 * management screen + the typeahead's owner). Extending the union is a
 * one-line change here + a render arm in `App.svelte`. */
export type AppRoute = "invoices" | "tenant" | "nav-credentials" | "partners";

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
    case "tenant":
      return "tenant";
    case "nav-credentials":
      return "nav-credentials";
    case "partners":
      return "partners";
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
