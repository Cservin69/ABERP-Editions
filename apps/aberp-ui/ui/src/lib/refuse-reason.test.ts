// S403 — pins for the operator REFUSE-with-reason validation contract.
// Mirrors the backend `validate_refuse_reason` (serve.rs) so the SPA
// never lets a reason through that the server would 400.

import { describe, expect, it } from "vitest";

import {
  REFUSE_REASON_MAX_CHARS,
  REFUSE_REASON_MIN_CHARS,
  validateRefuseReason,
} from "./refuse-reason";

const LF = String.fromCharCode(10);
const CR = String.fromCharCode(13);
const NUL = String.fromCharCode(0);
const TAB = String.fromCharCode(9);

describe("validateRefuseReason", () => {
  it("rejects empty / whitespace / under-floor reasons", () => {
    expect(validateRefuseReason("")).not.toBeNull();
    expect(validateRefuseReason("    ")).not.toBeNull();
    expect(validateRefuseReason("no")).not.toBeNull();
    // 4 chars after trim — still under the 5-char floor.
    expect(validateRefuseReason("  abcd  ")).not.toBeNull();
  });

  it("accepts a reason at or above the floor (and is trim-insensitive)", () => {
    expect(validateRefuseReason("abcde")).toBeNull();
    expect(validateRefuseReason("  stock shortfall  ")).toBeNull();
    // A plain space is NOT a control char — a normal multi-word reason passes.
    expect(validateRefuseReason("out of stock")).toBeNull();
    expect(REFUSE_REASON_MIN_CHARS).toBe(5);
  });

  it("rejects control chars (CR/LF/NUL/TAB) so the reason stays single-line", () => {
    expect(validateRefuseReason(`line1${LF}line2`)).not.toBeNull();
    expect(validateRefuseReason(`a${CR}b cd`)).not.toBeNull();
    expect(validateRefuseReason(`ab${NUL}cde`)).not.toBeNull();
    expect(validateRefuseReason(`tab${TAB}after`)).not.toBeNull();
  });

  it("rejects an over-long reason but accepts one exactly at the cap", () => {
    expect(validateRefuseReason("x".repeat(REFUSE_REASON_MAX_CHARS + 1))).not.toBeNull();
    expect(validateRefuseReason("y".repeat(REFUSE_REASON_MAX_CHARS))).toBeNull();
  });
});
