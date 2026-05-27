// PR-53 / session-73 — vitest pin for the hash-based router.
// Three routes (`invoices` / `tenant` / `nav-credentials`) + a
// default-fallback for unknown slugs. The router is a pure-module
// helper; vitest runs in jsdom so `window.location.hash` is
// available without a Tauri shell.

import { afterEach, describe, expect, it } from "vitest";

import {
  DEFAULT_ROUTE,
  HASH_PREFIX,
  parseRoute,
  routeHash,
  type AppRoute,
} from "./router";

describe("parseRoute", () => {
  it("maps the four canonical slugs verbatim", () => {
    const cases: { hash: string; expected: AppRoute }[] = [
      { hash: "#/invoices", expected: "invoices" },
      { hash: "#/tenant", expected: "tenant" },
      { hash: "#/nav-credentials", expected: "nav-credentials" },
      { hash: "#/partners", expected: "partners" },
    ];
    for (const { hash, expected } of cases) {
      expect(parseRoute(hash)).toBe(expected);
    }
  });

  it("tolerates hashes without the leading `#`", () => {
    expect(parseRoute("/invoices")).toBe("invoices");
    expect(parseRoute("invoices")).toBe("invoices");
  });

  it("falls back to the default for unknown slugs", () => {
    // An operator-typed bogus fragment should land on the invoice
    // list rather than an empty pane (CLAUDE.md rule 12 — surface
    // something concrete to the operator).
    expect(parseRoute("#/unknown")).toBe(DEFAULT_ROUTE);
    expect(parseRoute("")).toBe(DEFAULT_ROUTE);
  });

  it("strips trailing query strings before slug match", () => {
    // Future-proof: if a future widening adds `?param=x` semantics,
    // the slug match should still find the canonical route name.
    expect(parseRoute("#/tenant?foo=bar")).toBe("tenant");
  });
});

describe("routeHash", () => {
  it("composes the canonical hash form for each route", () => {
    expect(routeHash("invoices")).toBe(`${HASH_PREFIX}invoices`);
    expect(routeHash("tenant")).toBe(`${HASH_PREFIX}tenant`);
    expect(routeHash("nav-credentials")).toBe(`${HASH_PREFIX}nav-credentials`);
    expect(routeHash("partners")).toBe(`${HASH_PREFIX}partners`);
  });

  it("round-trips with parseRoute", () => {
    // The composer + parser are mirror pairs; a regression that
    // renames a slug in one without the other would surface here.
    const all: AppRoute[] = [
      "invoices",
      "tenant",
      "nav-credentials",
      "partners",
    ];
    for (const r of all) {
      expect(parseRoute(routeHash(r))).toBe(r);
    }
  });
});

afterEach(() => {
  // Clean up the global hash so a later test starting with
  // `currentRoute()` doesn't see leftover state.
  if (typeof window !== "undefined") {
    window.location.hash = "";
  }
});
