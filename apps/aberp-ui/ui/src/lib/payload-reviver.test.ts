// PR-35 / session-39 — vitest smoke test for `payload-reviver.ts`.
//
// Why this is the first SPA test. Session-37 (PR-33) and session-38
// (PR-34) both surfaced the `npm test` gate gap inline rather than
// closing it; the session-38 close handoff named the gap a cadence
// smell worth closing in PR-35. The session-32-and-37-and-38-named
// Option X picks `payload-reviver.ts` as the natural smoke-test
// target because the module is a pure function with no Svelte
// rendering surface (so the test stack stays at vitest + Node, no
// jsdom or svelte-testing-library needed).
//
// Why no jsdom. The session-38 handoff's option entry recommended
// "vitest + jsdom" as a default shape, but `payload-reviver.ts`'s
// only runtime dependency is `TextDecoder`, which is a Node global
// since v11 (and v18+ ships it as `globalThis.TextDecoder`). Adding
// jsdom would carry a transitive package surface (cssom, parse5,
// xmlchars, ...) for zero test coverage. Per CLAUDE.md rule 2
// (simplicity first) and rule 13 (delete the part). The trap is
// named here so a future SPA test that DOES need DOM (Svelte
// component rendering) will know to add jsdom or happy-dom at that
// PR's introduction site rather than as a pre-emptive devDep.
//
// Why no `vitest.config.ts`. Vitest's default config picks up
// `**/*.test.ts` automatically and uses the Node environment by
// default. The svelte-check tsconfig include `src/**/*.ts` already
// covers this file, so type-checking happens through the existing
// `npm run check` script too — no separate `tsconfig.test.json`
// fork needed. Per CLAUDE.md rule 2.

import { describe, expect, it } from "vitest";

import { bytesAsUtf8Replacer } from "./payload-reviver";

describe("bytesAsUtf8Replacer", () => {
  it("decodes a non-empty byte array of ASCII to its UTF-8 string", () => {
    expect(bytesAsUtf8Replacer("any", [72, 101, 108, 108, 111])).toBe(
      "Hello",
    );
  });

  it("decodes a non-ASCII UTF-8 byte sequence correctly", () => {
    // "Ár" = U+00C1 U+0072 → 0xC3 0x81 0x72 in UTF-8. The kind of
    // payload an ABERP-issued invoice's `request_xml` would carry
    // for a Hungarian-named customer.
    expect(bytesAsUtf8Replacer("any", [0xc3, 0x81, 0x72])).toBe("Ár");
  });

  it("preserves an empty array (zero-byte operator hint)", () => {
    // Per module doc: decoding `[]` to `""` would erase the operator-
    // visible hint that the field carried zero bytes.
    const empty: number[] = [];
    expect(bytesAsUtf8Replacer("any", empty)).toBe(empty);
  });

  it("passes through an array containing a non-integer element", () => {
    const arr = [72, 101.5, 108];
    expect(bytesAsUtf8Replacer("any", arr)).toBe(arr);
  });

  it("passes through an array containing an out-of-range integer", () => {
    const arr = [72, 256, 108];
    expect(bytesAsUtf8Replacer("any", arr)).toBe(arr);
  });

  it("passes through an array containing a negative integer", () => {
    const arr = [-1, 72, 108];
    expect(bytesAsUtf8Replacer("any", arr)).toBe(arr);
  });

  it("passes through an array containing a non-number element", () => {
    const arr = [72, "x", 108];
    expect(bytesAsUtf8Replacer("any", arr)).toBe(arr);
  });

  it("falls back to the int array when bytes are not valid UTF-8", () => {
    // 0xFF 0xFE is an invalid leading byte sequence under
    // `TextDecoder({ fatal: true })`; per module doc, the fallback
    // preserves information rather than emitting U+FFFD.
    const arr = [0xff, 0xfe, 0xfd];
    expect(bytesAsUtf8Replacer("any", arr)).toBe(arr);
  });

  it("passes through scalar values unchanged", () => {
    expect(bytesAsUtf8Replacer("k", "string")).toBe("string");
    expect(bytesAsUtf8Replacer("k", 42)).toBe(42);
    expect(bytesAsUtf8Replacer("k", null)).toBe(null);
    expect(bytesAsUtf8Replacer("k", true)).toBe(true);
  });

  it("passes through non-array objects unchanged", () => {
    const obj = { kind: "Submitted", request_xml: [72, 105] };
    // The replacer itself does NOT recurse — `JSON.stringify`
    // handles recursion. At the object node, the replacer returns
    // the object as-is so stringify keeps walking.
    expect(bytesAsUtf8Replacer("k", obj)).toBe(obj);
  });

  it("works end-to-end through JSON.stringify (documented use case)", () => {
    // The PR-27 audit-payload drill-down feeds typed payloads
    // through `JSON.stringify(payload, bytesAsUtf8Replacer, 2)`.
    // This test pins the integration shape: byte arrays substitute,
    // scalar fields stay structurally identical, nested objects
    // recurse through the replacer.
    //
    // Scalar `attempts: 3` is used in place of an array because the
    // module's heuristic intentionally over-decodes any non-empty
    // integer array in [0, 255] that happens to be valid UTF-8 (see
    // the "future drift" trap comment in `payload-reviver.ts`). A
    // small int array like `[1, 2, 3]` would decode to control
    // characters under the current heuristic — that is the
    // documented behaviour, not a regression to pin against.
    const payload = {
      kind: "Submitted",
      request_xml: [72, 105],
      attempts: 3,
      annotations: { reason: "test" },
    };
    const out = JSON.parse(JSON.stringify(payload, bytesAsUtf8Replacer));
    expect(out).toEqual({
      kind: "Submitted",
      request_xml: "Hi",
      attempts: 3,
      annotations: { reason: "test" },
    });
  });
});
