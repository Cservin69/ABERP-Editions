// PR-45a / session-61 — vitest pins on `boot-status.ts`. Per
// A163's mirror-invariant precedent the per-state view-mode table
// is the load-bearing operator-facing contract: a regression that
// rendered the loading pane while `status === "failed"` (or vice
// versa) would leave the operator staring at an animated indicator
// while the backend was actually dead.

import { describe, expect, it } from "vitest";

import type { BootStatusResponse } from "./api";
import {
  bootErrorMessage,
  bootViewMode,
  FAILURE_HINTS,
  latestLogLine,
} from "./boot-status";

describe("bootViewMode", () => {
  it("maps starting to loading", () => {
    expect(bootViewMode("starting")).toBe("loading");
  });

  it("maps ready to ready", () => {
    expect(bootViewMode("ready")).toBe("ready");
  });

  it("maps failed to error", () => {
    expect(bootViewMode("failed")).toBe("error");
  });

  // PR-46α / session-62 — the new needs-setup BootStatus variant
  // maps to a dedicated `setup` view-mode. The SPA's App.svelte
  // renders the first-run wizard against this mode. A regression
  // that collapsed needs-setup into either `loading` or `error`
  // would leave the operator stuck — either an infinite spinner
  // (loading) or an inaccurate failure pane (error).
  it("maps needs-setup to setup", () => {
    expect(bootViewMode("needs-setup")).toBe("setup");
  });

  // PR-51 / session-71 — the new needs-seller-config variant maps
  // to its own `seller-config` view-mode. Defence against a future
  // refactor that collapsed it into `setup` (which would mount the
  // NAV-creds wizard a second time) or `ready` (which would mount
  // the InvoiceList and let the operator hit the route surface
  // before the seller.toml is in place).
  it("maps needs-seller-config to seller-config", () => {
    expect(bootViewMode("needs-seller-config")).toBe("seller-config");
  });
});

describe("bootErrorMessage", () => {
  it("returns null when status is starting", () => {
    const snapshot: BootStatusResponse = {
      status: "starting",
      error: null,
      recent_logs: [],
    };
    expect(bootErrorMessage(snapshot)).toBeNull();
  });

  it("returns null when status is ready", () => {
    const snapshot: BootStatusResponse = {
      status: "ready",
      error: null,
      recent_logs: [],
    };
    expect(bootErrorMessage(snapshot)).toBeNull();
  });

  // PR-46α / session-62 — needs-setup is an operator-actionable
  // first-run step, NOT a failure. The error-message helper MUST
  // return null so the SPA does not render the failure pane on top
  // of the wizard.
  it("returns null when status is needs-setup", () => {
    const snapshot: BootStatusResponse = {
      status: "needs-setup",
      error: null,
      recent_logs: [],
    };
    expect(bootErrorMessage(snapshot)).toBeNull();
  });

  it("returns the verbatim error string when status is failed", () => {
    const snapshot: BootStatusResponse = {
      status: "failed",
      error:
        "spawn aberp serve subprocess: aberp serve did not print its handshake within 10s",
      recent_logs: [],
    };
    expect(bootErrorMessage(snapshot)).toBe(
      "spawn aberp serve subprocess: aberp serve did not print its handshake within 10s",
    );
  });

  it("falls back to a placeholder when status is failed and error is null", () => {
    // This shape would indicate a Rust-side bug — failed must carry
    // an error — but the helper still surfaces something loud per
    // rule 12 instead of returning null and showing nothing.
    const snapshot: BootStatusResponse = {
      status: "failed",
      error: null,
      recent_logs: [],
    };
    expect(bootErrorMessage(snapshot)).toBe(
      "backend boot failed with no error message",
    );
  });
});

describe("latestLogLine", () => {
  it("returns null when the ring buffer is empty", () => {
    const snapshot: BootStatusResponse = {
      status: "starting",
      error: null,
      recent_logs: [],
    };
    expect(latestLogLine(snapshot)).toBeNull();
  });

  it("returns the last line when the buffer has entries", () => {
    const snapshot: BootStatusResponse = {
      status: "starting",
      error: null,
      recent_logs: [
        "spawning aberp serve subprocess",
        "loopback TLS certificate ready",
        "binary hash compute (background) ready",
      ],
    };
    expect(latestLogLine(snapshot)).toBe(
      "binary hash compute (background) ready",
    );
  });
});

describe("FAILURE_HINTS", () => {
  it("surfaces at least the three brief-named common causes", () => {
    // The brief explicitly names "NAV credentials missing",
    // "database locked by another process", "port unavailable" as
    // the three common causes the error pane must hint at.
    const joined = FAILURE_HINTS.join(" ");
    expect(joined.toLowerCase()).toContain("nav credentials");
    expect(joined.toLowerCase()).toContain("database");
    expect(joined.toLowerCase()).toContain("port");
  });

  it("is non-empty and bounded so the pane stays compact", () => {
    expect(FAILURE_HINTS.length).toBeGreaterThan(0);
    expect(FAILURE_HINTS.length).toBeLessThan(8);
  });
});
