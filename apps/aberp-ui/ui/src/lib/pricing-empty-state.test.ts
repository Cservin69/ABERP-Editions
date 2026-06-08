import { describe, expect, it } from "vitest";
import { classifyEmptyState } from "./pricing-empty-state";
import type { PipelinePythonStatus } from "./api";

function resolved(daemonSpawned: boolean): PipelinePythonStatus {
  return {
    resolution_kind: "project_venv",
    resolved_path: "/repo/python/aberp-cad-extract/.venv/bin/python",
    module_importable: true,
    canonical_path: null,
    poll_cadence_secs: 60,
    daemon_spawned: daemonSpawned,
    recent_panic_count: 0,
    last_panic_at: null,
  };
}

function notResolved(): PipelinePythonStatus {
  return {
    resolution_kind: "not_resolved",
    resolved_path: null,
    module_importable: false,
    canonical_path: "/repo/python/aberp-cad-extract/.venv/bin/python",
    poll_cadence_secs: null,
    daemon_spawned: false,
    recent_panic_count: 0,
    last_panic_at: null,
  };
}

describe("classifyEmptyState — S282 / PR-267 empty-state forks", () => {
  it("rows present → 'rows' regardless of status", () => {
    expect(classifyEmptyState(5, resolved(true))).toBe("rows");
    expect(classifyEmptyState(1, notResolved())).toBe("rows");
    expect(classifyEmptyState(3, null)).toBe("rows");
  });

  it("no rows + not_resolved → 'venv_missing' (RED card)", () => {
    expect(classifyEmptyState(0, notResolved())).toBe("venv_missing");
  });

  it("no rows + resolved but daemon not spawned → 'spawn_errored' (AMBER)", () => {
    expect(classifyEmptyState(0, resolved(false))).toBe("spawn_errored");
  });

  it("no rows + resolved + daemon spawned → 'active' (GREEN)", () => {
    expect(classifyEmptyState(0, resolved(true))).toBe("active");
  });

  it("no rows + null status (e.g. status fetch failed) → 'active'", () => {
    // Status fetch fails gracefully via `.catch(() => null)`; the SPA
    // shouldn't disguise itself as broken just because the status
    // probe was flaky — the empty-state copy stays the original
    // "no pricing jobs in flight" line in that case.
    expect(classifyEmptyState(0, null)).toBe("active");
  });

  it("venv_missing takes precedence over spawn_errored when both apply", () => {
    // Defence-in-depth: if a future contributor accidentally emitted
    // resolution_kind=not_resolved with daemon_spawned=true (shouldn't
    // be possible — resolver guards it), we still surface the RED
    // copy because the underlying issue is the missing venv.
    const oddly: PipelinePythonStatus = {
      ...notResolved(),
      daemon_spawned: true,
    };
    expect(classifyEmptyState(0, oddly)).toBe("venv_missing");
  });
});
