// S433 — vitest pins for the Tenants admin view-model. The button-enable
// rules mirror the backend registry invariants; pinning them here proves
// the SPA disables exactly what the backend would refuse (refuse-running,
// refuse-only-active) without rendering Svelte.

import { describe, expect, it } from "vitest";

import type { TenantRow } from "./api";
import { buttonStateFor, orderTenants, visibleTenants } from "./tenants-list";

// Three tenants: one running (prod), one other Active (test), one
// Archived (old). This is the brief's SPA fixture.
const ROWS: TenantRow[] = [
  {
    slug: "prod",
    display_name: "ABEN AG",
    state: "active",
    created_at: "2026-06-16T04:46:00Z",
    running: true,
    nav_enabled: true,
  },
  {
    slug: "test",
    display_name: "ABEN Test",
    state: "active",
    created_at: "2026-06-16T04:47:00Z",
    running: false,
    nav_enabled: true,
  },
  {
    slug: "old",
    display_name: "Retired GmbH",
    state: "archived",
    created_at: "2026-01-01T00:00:00Z",
    running: false,
    nav_enabled: true,
  },
];

const find = (slug: string): TenantRow => {
  const r = ROWS.find((t) => t.slug === slug);
  if (!r) throw new Error(`no row ${slug}`);
  return r;
};

describe("buttonStateFor", () => {
  it("the running tenant can neither switch nor archive (must switch away first)", () => {
    const s = buttonStateFor(find("prod"), ROWS);
    expect(s.canSwitch).toBe(false);
    expect(s.canArchive).toBe(false);
    expect(s.canRestore).toBe(false);
  });

  it("a non-running Active tenant can switch + archive (two Active exist)", () => {
    const s = buttonStateFor(find("test"), ROWS);
    expect(s.canSwitch).toBe(true);
    expect(s.canArchive).toBe(true);
    expect(s.canRestore).toBe(false);
  });

  it("an Archived tenant can only restore", () => {
    const s = buttonStateFor(find("old"), ROWS);
    expect(s.canSwitch).toBe(false);
    expect(s.canArchive).toBe(false);
    expect(s.canRestore).toBe(true);
  });

  it("the demo tenant can switch but never archive or restore", () => {
    const withDemo: TenantRow[] = [
      ...ROWS,
      {
        slug: "demo",
        display_name: "Demo Tenant",
        state: "demo",
        created_at: "2026-06-16T00:00:00Z",
        running: false,
        nav_enabled: false,
      },
    ];
    const demo = withDemo[withDemo.length - 1];
    const s = buttonStateFor(demo, withDemo);
    expect(s.canSwitch).toBe(true);
    expect(s.canArchive).toBe(false);
    expect(s.canRestore).toBe(false);
  });

  it("the only Active tenant cannot be archived even when not running", () => {
    const solo: TenantRow[] = [
      { ...find("test"), running: false },
      { ...find("old") },
    ];
    // `test` is the sole Active; archiving it would leave zero Active.
    expect(buttonStateFor(solo[0], solo).canArchive).toBe(false);
  });
});

describe("orderTenants", () => {
  it("puts the running tenant first, then Active by slug, then Archived", () => {
    const ordered = orderTenants(ROWS).map((r) => r.slug);
    expect(ordered).toEqual(["prod", "test", "old"]);
  });

  it("does not mutate the input array", () => {
    const before = ROWS.map((r) => r.slug);
    orderTenants(ROWS);
    expect(ROWS.map((r) => r.slug)).toEqual(before);
  });
});

describe("visibleTenants (S434 hide-demo)", () => {
  const withDemo: TenantRow[] = [
    ...ROWS,
    {
      slug: "demo",
      display_name: "Demo Tenant",
      state: "demo",
      created_at: "2026-06-16T00:00:00Z",
      running: false,
      nav_enabled: false,
    },
  ];

  it("shows demo when hide_demo is off", () => {
    const slugs = visibleTenants(withDemo, false, true).map((r) => r.slug);
    expect(slugs).toContain("demo");
  });

  it("hides demo when hide_demo on AND a real tenant exists", () => {
    const slugs = visibleTenants(withDemo, true, true).map((r) => r.slug);
    expect(slugs).not.toContain("demo");
    expect(slugs).toContain("prod");
  });

  it("never hides demo when no real tenant exists (fresh install)", () => {
    const demoOnly: TenantRow[] = [withDemo[withDemo.length - 1]];
    const slugs = visibleTenants(demoOnly, true, false).map((r) => r.slug);
    expect(slugs).toContain("demo");
  });

  it("never hides the demo when it is the running tenant", () => {
    const runningDemo = withDemo.map((r) =>
      r.slug === "demo" ? { ...r, running: true } : { ...r, running: false },
    );
    const slugs = visibleTenants(runningDemo, true, true).map((r) => r.slug);
    expect(slugs).toContain("demo");
  });
});
