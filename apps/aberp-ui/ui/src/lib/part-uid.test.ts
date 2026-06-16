// S438 — vitest pins for the part-UID client-side validators.

import { describe, it, expect } from "vitest";

import { validateSerial, validatePartUid } from "./part-uid";

describe("validateSerial", () => {
  it("accepts an empty value (server auto-derives the serial)", () => {
    expect(validateSerial("")).toBeNull();
    expect(validateSerial("   ")).toBeNull();
  });

  it("accepts a normal serial", () => {
    expect(validateSerial("SN-001")).toBeNull();
    expect(validateSerial("LOT/42.A")).toBeNull();
    expect(validateSerial("  SN-9  ")).toBeNull();
  });

  it("rejects a serial carrying the DataMatrix delimiter", () => {
    expect(validateSerial("A|B")).not.toBeNull();
  });

  it("rejects a serial longer than 64 chars", () => {
    expect(validateSerial("x".repeat(64))).toBeNull();
    expect(validateSerial("x".repeat(65))).not.toBeNull();
  });

  it("rejects control characters", () => {
    expect(validateSerial("SN\x01")).not.toBeNull();
    expect(validateSerial("SN\t1")).not.toBeNull();
  });
});

describe("validatePartUid", () => {
  it("accepts a dp- + 26-char Crockford ULID", () => {
    expect(validatePartUid("dp-01ARZ3NDEKTSV4RRFFQ69G5FAV")).toBeNull();
  });

  it("rejects a missing dp- prefix", () => {
    expect(validatePartUid("01ARZ3NDEKTSV4RRFFQ69G5FAV")).not.toBeNull();
  });

  it("rejects a wrong-length body", () => {
    expect(validatePartUid("dp-tooshort")).not.toBeNull();
  });

  it("rejects non-Crockford chars (I/L/O/U)", () => {
    expect(validatePartUid("dp-ILOU3NDEKTSV4RRFFQ69G5FAV")).not.toBeNull();
  });
});
