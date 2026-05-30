// PR-89 / ADR-0045 — vitest pins for the SPA-side numbering helper.
// Mirrors the load-bearing invariants pinned in the Rust
// `apps/aberp/src/numbering.rs::tests` module: pad-as-floor overflow,
// reorder render order, exactly-one-counter, NAV-charset rejection,
// annual-reset gate, Ervin's primary shape, default template stability,
// move/remove pure helpers.

import { describe, expect, test } from "vitest";
import {
  defaultTemplate,
  errorMessage,
  invoiceNumberBuildPrefix,
  moveSegmentDown,
  moveSegmentUp,
  removeSegment,
  renderTemplate,
  renderTemplateForBuild,
  validateTemplate,
  type NumberingSegment,
  type NumberingTemplate,
} from "./invoice-numbering";

describe("S165 build-profile prefix", () => {
  // The "feature flag check" is `isProductionBuild`, threaded into the
  // SPA from `GET /health`. Mock it by passing the boolean directly.
  const t: NumberingTemplate = {
    segments: [
      { kind: "Literal", text: "ABERP/" },
      { kind: "Year", digits: 4 },
      { kind: "Literal", text: "/" },
      { kind: "Counter", pad_width: 4 },
    ],
    reset_policy: "on_year_change",
    start_value: 1,
  };

  test("dev/test build (feature OFF) prepends TEST-", () => {
    expect(invoiceNumberBuildPrefix(false)).toBe("TEST-");
    expect(renderTemplateForBuild(t, 2026, 42, false)).toBe(
      "TEST-ABERP/2026/0042",
    );
  });

  test("production build (feature ON) omits the prefix", () => {
    expect(invoiceNumberBuildPrefix(true)).toBe("");
    expect(renderTemplateForBuild(t, 2026, 42, true)).toBe("ABERP/2026/0042");
    // …and matches the bare pure render exactly.
    expect(renderTemplateForBuild(t, 2026, 42, true)).toBe(
      renderTemplate(t, 2026, 42),
    );
  });
});

describe("default template", () => {
  test("renders the pre-PR-89 INV-default/NNNNN shape byte-for-byte", () => {
    const t = defaultTemplate();
    expect(renderTemplate(t, 2026, 1)).toBe("INV-default/00001");
    expect(renderTemplate(t, 2026, 42)).toBe("INV-default/00042");
    expect(renderTemplate(t, 2026, 99999)).toBe("INV-default/99999");
  });

  test("validates clean", () => {
    expect(validateTemplate(defaultTemplate())).toBeNull();
  });
});

describe("counter pad-as-floor (overflow grows)", () => {
  // Ervin's named case: width-2 renders 01..99..100..101, never truncates.
  test("width-2 counter renders 01..99..100..101", () => {
    const t: NumberingTemplate = {
      segments: [{ kind: "Counter", pad_width: 2 }],
      reset_policy: "never",
      start_value: 1,
    };
    expect(renderTemplate(t, 2026, 1)).toBe("01");
    expect(renderTemplate(t, 2026, 9)).toBe("09");
    expect(renderTemplate(t, 2026, 99)).toBe("99");
    expect(renderTemplate(t, 2026, 100)).toBe("100");
    expect(renderTemplate(t, 2026, 101)).toBe("101");
    expect(renderTemplate(t, 2026, 99999)).toBe("99999");
  });
});

describe("Year segment digit width", () => {
  test("2-digit Year renders YY", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Year", digits: 2 },
        { kind: "Counter", pad_width: 4 },
      ],
      reset_policy: "never",
      start_value: 1,
    };
    expect(renderTemplate(t, 2026, 7)).toBe("260007");
  });

  test("4-digit Year renders YYYY", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Year", digits: 4 },
        { kind: "Counter", pad_width: 4 },
      ],
      reset_policy: "never",
      start_value: 1,
    };
    expect(renderTemplate(t, 2026, 7)).toBe("20260007");
  });
});

describe("segment order drives render order", () => {
  // Ervin's `0001/2026-ABEDIFFERENT` shape from the spec.
  test("reorder renders in declaration order", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Counter", pad_width: 4 },
        { kind: "Literal", text: "/" },
        { kind: "Year", digits: 4 },
        { kind: "Literal", text: "-ABEDIFFERENT" },
      ],
      reset_policy: "never",
      start_value: 1,
    };
    expect(renderTemplate(t, 2026, 1)).toBe("0001/2026-ABEDIFFERENT");
  });
});

describe("validator — exactly-one-counter invariants", () => {
  test("zero counters loud-fails", () => {
    const t: NumberingTemplate = {
      segments: [{ kind: "Literal", text: "ABERP-" }],
      reset_policy: "never",
      start_value: 1,
    };
    expect(validateTemplate(t)?.kind).toBe("NoCounter");
  });

  test("multiple counters loud-fails with count", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Counter", pad_width: 2 },
        { kind: "Literal", text: "-" },
        { kind: "Counter", pad_width: 2 },
      ],
      reset_policy: "never",
      start_value: 1,
    };
    const err = validateTemplate(t);
    expect(err?.kind).toBe("MultipleCounters");
    if (err?.kind === "MultipleCounters") {
      expect(err.count).toBe(2);
    }
  });
});

describe("validator — NAV invoiceNumber charset", () => {
  test("backslash in Literal loud-fails", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Literal", text: "ABERP\\" },
        { kind: "Counter", pad_width: 4 },
      ],
      reset_policy: "never",
      start_value: 1,
    };
    const err = validateTemplate(t);
    expect(err?.kind).toBe("InvalidLiteralCharacter");
    if (err?.kind === "InvalidLiteralCharacter") {
      expect(err.character).toBe("\\");
      expect(err.segmentIndex).toBe(0);
    }
  });

  test("other NAV-illegal characters loud-fail", () => {
    for (const bad of [" ", ".", "_", "#", "@"]) {
      const t: NumberingTemplate = {
        segments: [
          { kind: "Literal", text: `ABE${bad}RP` },
          { kind: "Counter", pad_width: 4 },
        ],
        reset_policy: "never",
        start_value: 1,
      };
      expect(validateTemplate(t)?.kind).toBe("InvalidLiteralCharacter");
    }
  });

  test("NAV-legal special characters accepted (dash + slash + alphanum mix)", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Literal", text: "AB-ERP/2026-" },
        { kind: "Counter", pad_width: 6 },
      ],
      reset_policy: "never",
      start_value: 1,
    };
    expect(validateTemplate(t)).toBeNull();
  });
});

describe("validator — reset-policy / Year gate", () => {
  test("on_year_change without Year segment loud-fails", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Literal", text: "ABERP/" },
        { kind: "Counter", pad_width: 6 },
      ],
      reset_policy: "on_year_change",
      start_value: 1,
    };
    expect(validateTemplate(t)?.kind).toBe("OnYearChangeWithoutYearSegment");
  });

  test("on_year_change WITH Year segment validates clean", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Literal", text: "ABERP-" },
        { kind: "Year", digits: 4 },
        { kind: "Literal", text: "/" },
        { kind: "Counter", pad_width: 6 },
      ],
      reset_policy: "on_year_change",
      start_value: 1,
    };
    expect(validateTemplate(t)).toBeNull();
  });
});

describe("validator — degenerate inputs", () => {
  test("empty template loud-fails", () => {
    expect(
      validateTemplate({ segments: [], reset_policy: "never", start_value: 1 })?.kind,
    ).toBe("EmptyTemplate");
  });

  test("empty Literal loud-fails", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Literal", text: "" },
        { kind: "Counter", pad_width: 4 },
      ],
      reset_policy: "never",
      start_value: 1,
    };
    expect(validateTemplate(t)?.kind).toBe("EmptyLiteral");
  });

  test("zero start_value loud-fails", () => {
    const t: NumberingTemplate = {
      segments: [{ kind: "Counter", pad_width: 4 }],
      reset_policy: "never",
      start_value: 0,
    };
    expect(validateTemplate(t)?.kind).toBe("InvalidStartValue");
  });
});

describe("Ervin's primary template (go-live shape)", () => {
  test("ABERP-2026/000001 renders + annual-reset visualizes 2027 reset", () => {
    const t: NumberingTemplate = {
      segments: [
        { kind: "Literal", text: "ABERP-" },
        { kind: "Year", digits: 4 },
        { kind: "Literal", text: "/" },
        { kind: "Counter", pad_width: 6 },
      ],
      reset_policy: "on_year_change",
      start_value: 1,
    };
    expect(validateTemplate(t)).toBeNull();
    expect(renderTemplate(t, 2026, 1)).toBe("ABERP-2026/000001");
    expect(renderTemplate(t, 2026, 1247)).toBe("ABERP-2026/001247");
    expect(renderTemplate(t, 2027, 1)).toBe("ABERP-2027/000001");
  });
});

describe("error messages are bilingual", () => {
  test("every error variant carries Hungarian + English", () => {
    const cases = [
      { kind: "EmptyTemplate" as const },
      { kind: "NoCounter" as const },
      { kind: "MultipleCounters" as const, count: 3 },
      { kind: "EmptyLiteral" as const, segmentIndex: 0 },
      {
        kind: "InvalidLiteralCharacter" as const,
        segmentIndex: 0,
        character: "\\",
      },
      { kind: "TooLong" as const, renderedMinLen: 60 },
      { kind: "OnYearChangeWithoutYearSegment" as const },
      { kind: "InvalidStartValue" as const },
    ];
    for (const err of cases) {
      const msg = errorMessage(err);
      // Bilingual marker: each message contains a newline separating
      // the two language halves.
      expect(msg).toContain("\n");
      expect(msg.length).toBeGreaterThan(20);
    }
  });
});

describe("segment-reorder helpers", () => {
  const a: NumberingSegment = { kind: "Literal", text: "A" };
  const b: NumberingSegment = { kind: "Year", digits: 4 };
  const c: NumberingSegment = { kind: "Counter", pad_width: 4 };

  test("moveSegmentUp swaps with previous", () => {
    const next = moveSegmentUp([a, b, c], 1);
    expect(next).toEqual([b, a, c]);
  });

  test("moveSegmentUp at index 0 is a no-op", () => {
    const segs = [a, b, c];
    expect(moveSegmentUp(segs, 0)).toBe(segs);
  });

  test("moveSegmentDown swaps with next", () => {
    const next = moveSegmentDown([a, b, c], 1);
    expect(next).toEqual([a, c, b]);
  });

  test("moveSegmentDown at last index is a no-op", () => {
    const segs = [a, b, c];
    expect(moveSegmentDown(segs, 2)).toBe(segs);
  });

  test("removeSegment removes by index", () => {
    expect(removeSegment([a, b, c], 1)).toEqual([a, c]);
  });

  test("removeSegment is pure (does not mutate input)", () => {
    const segs = [a, b, c];
    const next = removeSegment(segs, 0);
    expect(segs.length).toBe(3);
    expect(next.length).toBe(2);
  });
});
