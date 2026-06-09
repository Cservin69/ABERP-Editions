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

  // ── PR-274 / S297 F6 — MarginFloor badge copy split ────────────────

  it("returns 'Operator review required' for permanent + MarginFloor reason", () => {
    // The engine's `QuoteError::MarginFloorViolation` Display contains
    // "below configured floor". Retry alone won't fix it — the operator
    // must edit Quoting Parameters first, so the badge copy MUST tell
    // them that, not the generic "Operator retry required".
    const reason =
      "computed margin 0.0500 below configured floor 0.1500 (total_price=10.0000)";
    const b = failureKindBadge("permanent", reason);
    expect(b).not.toBeNull();
    expect(b!.className).toBe("chip chip--err");
    expect(b!.label).toContain("Operator review required");
    expect(b!.label).toContain("Operátor felülvizsgálat szükséges");
    // NOT the generic copy — that would re-introduce the misdirection
    // S296 F6 named.
    expect(b!.label).not.toContain("Operator retry required");
  });

  it("returns 'Operator retry required' for permanent + non-MarginFloor reason", () => {
    // STEP-assembly rejection is Permanent but the operator action IS
    // a retry-after-re-upload, so the generic copy is correct here.
    const reason =
      "subprocess exited with code Some(2): ValueError: STEP file contains an assembly with 3 solids";
    const b = failureKindBadge("permanent", reason);
    expect(b).not.toBeNull();
    expect(b!.label).toContain("Operator retry required");
    expect(b!.label).not.toContain("Operator review required");
  });

  it("MarginFloor copy is case-insensitive on the error reason", () => {
    // The Rust classifier lowercases before matching; the SPA mirrors
    // that so an uppercase reason from any future engine version still
    // lands on the review-required copy.
    const upper =
      "Computed Margin 0.0500 BELOW CONFIGURED FLOOR 0.1500 (total_price=10.0000)";
    const b = failureKindBadge("permanent", upper);
    expect(b).not.toBeNull();
    expect(b!.label).toContain("Operator review required");
  });

  it("returns generic permanent copy when error reason is omitted", () => {
    // Backwards-compatible single-arg form — callers without the raw
    // reason still get a sensible (if conservative) badge.
    const b = failureKindBadge("permanent");
    expect(b).not.toBeNull();
    expect(b!.label).toContain("Operator retry required");
  });

  it("returns generic permanent copy when error reason is null", () => {
    // Legacy rows + rows with a missing reason field both fall back
    // to the generic copy rather than misclassifying as MarginFloor.
    const b = failureKindBadge("permanent", null);
    expect(b).not.toBeNull();
    expect(b!.label).toContain("Operator retry required");
  });

  it("MarginFloor copy does not change Transient or Unknown verdicts", () => {
    // Defence-in-depth — the reason hint must only re-route the
    // Permanent verdict; Transient + Unknown stay independent of
    // reason text so cross-talk can't shift the className.
    const reason =
      "computed margin 0.0500 below configured floor 0.1500 (total_price=10.0000)";
    expect(failureKindBadge("transient", reason)!.className).toBe(
      "chip chip--running",
    );
    expect(failureKindBadge("unknown", reason)!.className).toBe(
      "chip chip--queued",
    );
    expect(failureKindBadge(null, reason)!.label).toContain("Unknown");
  });
});
