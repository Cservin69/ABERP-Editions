// PR-36 / session-40 — vitest smoke test for the new `ACK_LABELS`
// table and `ackLabelMeta` lookup (Option Y).
//
// The TS Record shape on `ACK_LABELS: Record<AckStatus, LabelMeta>`
// is the load-bearing TYPE-level enforcement (a missing key fails
// `npm run check` per ADR-0036 §7's discipline). This file pins the
// same invariant at the TEST level so a future label table refactor
// that loses the strict `Record<AckStatus, _>` typing (e.g.,
// switching to `Partial<Record<...>>` or a `Map`) still fails the
// gate. Also pins the fallback behaviour per CLAUDE.md rule 12 (fail
// loud) — an unknown ack must yield a visible muted "?" pill rather
// than throw or silently bucket with a known value.
//
// PR-38 / session-42 — extended to cover the `LABELS` /
// `LIFECYCLE_ORDER` invariants (Option BB). The TS
// `Record<InvoiceState, LabelMeta>` shape on `LABELS` pins
// exhaustiveness at the type level; the `as const satisfies readonly
// InvoiceState[]` on `LIFECYCLE_ORDER` pins per-entry membership; but
// the dedup + length + set-equality invariants between the two are
// NOT expressible in TS alone. Pre-PR-38 those three checks were
// enforced only by the module-load `throw` block in `labels.ts` per
// CLAUDE.md rule 12 (fail loud at app-startup). PR-38 lifts the
// failure surface from app-startup to gate-time using the same
// two-way-pin pattern PR-36 established for `ACK_LABELS` (A128) — a
// local `INVOICE_STATE_LITERALS` constant mirrors the eleven labels
// per ADR-0036 §2 verbatim, so a drift in either direction fails
// `npm test`.
//
// PR-40 / session-44 — extended to cover the `labelMeta` and
// `lifecycleIndex` lookup-function surfaces (Option DD). The lookups
// already had INDIRECT coverage via the table-shape invariants
// (LABELS / LIFECYCLE_ORDER drift breaks the consumers), but the
// fallback paths (unknown-state, empty-string) were untested at
// vitest and enforced only by the explicit `if (i === -1)` and
// `if (known !== undefined)` branches in `labels.ts`. Pinned here so
// a future refactor that drops the fallback — returning a table miss
// as `0` would silently mis-sort unknown rows at the top of the list
// rather than at the bottom; returning `undefined` would crash the
// renderer — fails at gate-time. Mirrors the `ackLabelMeta` cases
// (known round-trip + unknown-string + empty-string) per CLAUDE.md
// rule 11 (match conventions). Closes the labels.ts test surface
// end-to-end: every export from labels.ts is now vitest-pinned.
//
// No jsdom needed (labels.ts is a pure-data module — same posture as
// the PR-35 payload-reviver.test.ts).

import { describe, expect, it } from "vitest";

import type { AckStatus, InvoiceState } from "./api";
import {
  ACK_LABELS,
  LABELS,
  LIFECYCLE_ORDER,
  ackLabelMeta,
  labelMeta,
  lifecycleIndex,
} from "./labels";

// The four NAV v3.0 `processingResult` literals per ADR-0009 §2.
// Mirrored verbatim here so the test pins the wire literals as well
// as the count — a drift in `AckStatus` (rename, new variant) that
// is NOT reflected here fails this test, AND a drift here that is
// NOT reflected in `ACK_LABELS` fails the `Record<AckStatus, ...>`
// type check. Two-way pin.
const ACK_STATUS_LITERALS: readonly AckStatus[] = [
  "RECEIVED",
  "PROCESSING",
  "SAVED",
  "ABORTED",
] as const;

describe("ACK_LABELS", () => {
  it("has an entry for every AckStatus literal (set-equality)", () => {
    const tableKeys = Object.keys(ACK_LABELS).sort();
    const unionKeys = [...ACK_STATUS_LITERALS].sort();
    expect(tableKeys).toEqual(unionKeys);
  });

  it("assigns a non-empty icon and tooltip to every entry", () => {
    for (const status of ACK_STATUS_LITERALS) {
      const meta = ACK_LABELS[status];
      expect(meta.icon.length).toBeGreaterThan(0);
      expect(meta.tooltip.length).toBeGreaterThan(0);
    }
  });

  it("assigns the expected signal class to each terminal value", () => {
    // The two terminal acks mirror the lifecycle's terminal labels:
    //   SAVED ↔ Finalized (positive)
    //   ABORTED ↔ Rejected (negative)
    // Pinned so a future signal-table edit that swaps these surfaces
    // at gate-time rather than via an operator misread of the chip.
    expect(ACK_LABELS.SAVED.signal).toBe("positive");
    expect(ACK_LABELS.ABORTED.signal).toBe("negative");
  });

  it("assigns the warning signal to both intermediate values", () => {
    // RECEIVED and PROCESSING are both "still in flight" from the
    // operator's perspective — the Submitted lifecycle chip
    // collapses both, and the per-ack chip surfaces which stage NAV
    // is at without changing the signal colour.
    expect(ACK_LABELS.RECEIVED.signal).toBe("warning");
    expect(ACK_LABELS.PROCESSING.signal).toBe("warning");
  });
});

describe("ackLabelMeta", () => {
  it("returns the table entry verbatim for each known AckStatus", () => {
    for (const status of ACK_STATUS_LITERALS) {
      expect(ackLabelMeta(status)).toBe(ACK_LABELS[status]);
    }
  });

  it("returns a muted fallback for an unknown string", () => {
    // CLAUDE.md rule 12 — an ack literal the SPA does not model
    // surfaces visibly. The renderer paints the muted "?" pill
    // rather than throwing, so the operator can still inspect the
    // raw string while the divergence is named in the tooltip.
    const fallback = ackLabelMeta("DONE");
    expect(fallback.signal).toBe("muted");
    expect(fallback.icon).toBe("?");
    expect(fallback.tooltip).toContain("DONE");
  });

  it("returns the muted fallback for the empty string", () => {
    // Defensive: a backend that emits a stripped/whitespace ack
    // (would indicate a parser bug) still produces a visible muted
    // pill rather than an empty-text chip.
    const fallback = ackLabelMeta("");
    expect(fallback.signal).toBe("muted");
    expect(fallback.icon).toBe("?");
  });
});

// The eleven derive_state labels per ADR-0036 §2. Mirrored verbatim
// here so the test pins the wire literals as well as the count — a
// drift in `InvoiceState` (rename, add, drop) that is NOT reflected
// here fails the LABELS / LIFECYCLE_ORDER set-equality cases below,
// AND a drift here that is NOT reflected in `LABELS` fails the
// `Record<InvoiceState, ...>` type check at `npm run check`. Two-way
// pin per A128, mirror of the `ACK_STATUS_LITERALS` constant above.
const INVOICE_STATE_LITERALS: readonly InvoiceState[] = [
  "Unknown",
  "Ready",
  "Pending",
  "PendingNavExists",
  "Submitted",
  "Recovered",
  "Finalized",
  "Rejected",
  "Storno",
  "Amended",
  "Abandoned",
] as const;

describe("LABELS", () => {
  it("has an entry for every InvoiceState literal (set-equality)", () => {
    const tableKeys = Object.keys(LABELS).sort();
    const unionKeys = [...INVOICE_STATE_LITERALS].sort();
    expect(tableKeys).toEqual(unionKeys);
  });

  it("assigns a non-empty icon and tooltip to every entry", () => {
    for (const state of INVOICE_STATE_LITERALS) {
      const meta = LABELS[state];
      expect(meta.icon.length).toBeGreaterThan(0);
      expect(meta.tooltip.length).toBeGreaterThan(0);
    }
  });
});

describe("LIFECYCLE_ORDER", () => {
  it("contains every InvoiceState literal exactly once (set-equality)", () => {
    // Mirrors the labels.ts module-load throw's set-equality check
    // at gate-time. A future PR that adds a label to LABELS but
    // forgets LIFECYCLE_ORDER fails here BEFORE app-startup; same
    // for the reverse.
    const orderKeys = [...LIFECYCLE_ORDER].sort();
    const unionKeys = [...INVOICE_STATE_LITERALS].sort();
    expect(orderKeys).toEqual(unionKeys);
  });

  it("contains no duplicates", () => {
    // Mirrors the labels.ts module-load throw's dedup check at
    // gate-time. A typo that duplicates a label (and silently
    // drops another) is caught explicitly with a clear failure
    // message rather than via the indirect set-equality miss.
    expect(new Set(LIFECYCLE_ORDER).size).toBe(LIFECYCLE_ORDER.length);
  });

  it("has the same length as the LABELS table key count", () => {
    // Mirrors the labels.ts module-load throw's length check at
    // gate-time. Redundant with set-equality + dedup but pinned
    // explicitly so the invariant is named for future readers.
    expect(LIFECYCLE_ORDER.length).toBe(Object.keys(LABELS).length);
  });
});

describe("labelMeta", () => {
  it("returns the table entry verbatim for each known InvoiceState", () => {
    for (const state of INVOICE_STATE_LITERALS) {
      expect(labelMeta(state)).toBe(LABELS[state]);
    }
  });

  it("returns a muted fallback for an unknown string", () => {
    // CLAUDE.md rule 12 — a state literal the SPA does not model
    // surfaces visibly. The renderer paints the muted "?" pill so
    // the operator can still read the raw string while the
    // divergence is named in the tooltip.
    const fallback = labelMeta("Drafted");
    expect(fallback.signal).toBe("muted");
    expect(fallback.icon).toBe("?");
    expect(fallback.tooltip).toContain("Drafted");
  });

  it("returns the muted fallback for the empty string", () => {
    // Defensive: a backend that emits a stripped/whitespace label
    // (would indicate a parser bug) still produces a visible muted
    // pill rather than an empty-text chip.
    const fallback = labelMeta("");
    expect(fallback.signal).toBe("muted");
    expect(fallback.icon).toBe("?");
  });
});

describe("lifecycleIndex", () => {
  it("returns the LIFECYCLE_ORDER position for each known InvoiceState", () => {
    // Round-trip pin: lifecycleIndex(s) === LIFECYCLE_ORDER.indexOf(s)
    // for every known label, AND every index falls inside the array.
    // A future refactor that re-sorts inside lifecycleIndex without
    // rebuilding LIFECYCLE_ORDER (or vice versa) fails here before
    // the sorted list reaches the operator.
    for (const state of INVOICE_STATE_LITERALS) {
      const expected = (LIFECYCLE_ORDER as readonly string[]).indexOf(state);
      expect(lifecycleIndex(state)).toBe(expected);
      expect(lifecycleIndex(state)).toBeGreaterThanOrEqual(0);
      expect(lifecycleIndex(state)).toBeLessThan(LIFECYCLE_ORDER.length);
    }
  });

  it("returns LIFECYCLE_ORDER.length for unknown strings (visible at bottom)", () => {
    // CLAUDE.md rule 12 — the backend invented a state without a SPA
    // update; the row sorts AFTER every known label so it stays
    // visible at the bottom of the table rather than silently
    // bucketing with `Unknown` (which sorts at index 0). The empty
    // string takes the same path.
    expect(lifecycleIndex("Drafted")).toBe(LIFECYCLE_ORDER.length);
    expect(lifecycleIndex("")).toBe(LIFECYCLE_ORDER.length);
  });
});
