// S290 / PR-271 — vitest pins for the failure-kind badge classifier.

import { describe, it, expect } from "vitest";

import { failureKindBadge } from "./pricing-failure-kind";

describe("failureKindBadge", () => {
  it("returns RED operator-action badge for permanent failures", () => {
    const b = failureKindBadge("permanent");
    expect(b).not.toBeNull();
    expect(b!.className).toBe("chip chip--err");
    // The bilingual label is the operator's only signal that clicking
    // Retry won't help — the EN half must say so explicitly.
    expect(b!.label).toContain("Operator retry required");
    expect(b!.label).toContain("Operátor művelet szükséges");
  });

  it("returns AMBER auto-retry badge for transient failures", () => {
    const b = failureKindBadge("transient");
    expect(b).not.toBeNull();
    expect(b!.className).toBe("chip chip--running");
    expect(b!.label).toContain("Auto-retry");
  });

  it("returns NEUTRAL unknown badge for explicit unknown", () => {
    const b = failureKindBadge("unknown");
    expect(b).not.toBeNull();
    expect(b!.className).toBe("chip chip--queued");
    expect(b!.label).toContain("Ismeretlen");
    expect(b!.label).toContain("Unknown");
  });

  it("treats legacy null the same as unknown", () => {
    // PROD_v2.27.[0-5] Failed rows that pre-date the classifier write
    // `failure_kind = null`. The badge must NOT lie about retry
    // prospects — neutral, not RED.
    const b = failureKindBadge(null);
    expect(b).not.toBeNull();
    expect(b!.className).toBe("chip chip--queued");
    expect(b!.label).toContain("Unknown");
  });

  it("surfaces an unknown verbatim string instead of dropping it", () => {
    // Defence-in-depth: future backend version emits a new vocab value.
    // Don't pretend the row is fine — show the value as-is so the
    // operator sees something to file a bug about.
    const b = failureKindBadge("not_a_known_kind");
    expect(b).not.toBeNull();
    expect(b!.label).toBe("not_a_known_kind");
  });

  it("uses pairwise-distinct className per known verdict", () => {
    // A collision would mute the operator's "you shouldn't bother
    // clicking Retry" signal. CLAUDE.md rule 12 — fail loud.
    const permanent = failureKindBadge("permanent")!.className;
    const transient = failureKindBadge("transient")!.className;
    const unknown = failureKindBadge("unknown")!.className;
    expect(permanent).not.toBe(transient);
    expect(permanent).not.toBe(unknown);
    expect(transient).not.toBe(unknown);
  });
});
