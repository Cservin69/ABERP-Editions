// PR-84 — pins for the pure date helpers in `invoice-dates.ts`.
//
// These three families of helpers are load-bearing for the issue
// form's UX:
//   - addDays / daysBetween — bidirectional payment-deadline calc
//   - comfortZone           — delivery-date classification
//   - parseIsoDate          — input validation (catches "2026-02-30")
//
// Date arithmetic is easy to get subtly wrong at month/year/leap-year
// boundaries. The pins exercise each boundary so a future refactor
// (e.g., swapping the UTC noon convention) surfaces here rather than
// at a regulatory-misfile cost.

import { describe, expect, it } from "vitest";

import {
  addDays,
  comfortZone,
  daysBetween,
  overrideKindForZone,
  parseIsoDate,
  todayLocalIso,
} from "./invoice-dates";

describe("parseIsoDate", () => {
  it("accepts a well-formed YYYY-MM-DD string", () => {
    expect(parseIsoDate("2026-05-27")).toEqual({ y: 2026, m: 5, d: 27 });
  });

  it("rejects a malformed string", () => {
    expect(parseIsoDate("2026-5-27")).toBeNull();
    expect(parseIsoDate("2026/05/27")).toBeNull();
    expect(parseIsoDate("")).toBeNull();
    expect(parseIsoDate("not-a-date")).toBeNull();
  });

  it("rejects a date that doesn't exist on the calendar (2026-02-30)", () => {
    // JS Date silently turns 2026-02-30 into 2026-03-02 — the helper
    // catches the overflow so the form's input validator surfaces a
    // precise error.
    expect(parseIsoDate("2026-02-30")).toBeNull();
    expect(parseIsoDate("2025-02-29")).toBeNull(); // not a leap year
    expect(parseIsoDate("2024-02-29")).toEqual({ y: 2024, m: 2, d: 29 }); // leap
  });

  it("rejects out-of-range month or day", () => {
    expect(parseIsoDate("2026-00-15")).toBeNull();
    expect(parseIsoDate("2026-13-15")).toBeNull();
    expect(parseIsoDate("2026-05-00")).toBeNull();
    expect(parseIsoDate("2026-05-32")).toBeNull();
  });
});

describe("addDays", () => {
  it("adds N days within a month", () => {
    expect(addDays("2026-05-01", 10)).toBe("2026-05-11");
  });

  it("crosses a month boundary correctly", () => {
    expect(addDays("2026-05-27", 8)).toBe("2026-06-04");
  });

  it("crosses a year boundary correctly", () => {
    expect(addDays("2026-12-25", 10)).toBe("2027-01-04");
  });

  it("subtracts with a negative offset", () => {
    expect(addDays("2026-06-04", -8)).toBe("2026-05-27");
  });

  it("returns the same date for offset 0", () => {
    expect(addDays("2026-05-27", 0)).toBe("2026-05-27");
  });

  it("handles leap-day arithmetic", () => {
    expect(addDays("2024-02-28", 1)).toBe("2024-02-29");
    expect(addDays("2024-02-29", 1)).toBe("2024-03-01");
    expect(addDays("2025-02-28", 1)).toBe("2025-03-01"); // not a leap year
  });

  it("returns null on malformed input", () => {
    expect(addDays("not-a-date", 1)).toBeNull();
    expect(addDays("2026-02-30", 1)).toBeNull();
    expect(addDays("2026-05-27", Number.NaN)).toBeNull();
    expect(addDays("2026-05-27", 1.5)).toBeNull();
  });
});

describe("daysBetween", () => {
  it("returns positive when toIso is after fromIso", () => {
    expect(daysBetween("2026-05-27", "2026-06-04")).toBe(8);
  });

  it("returns zero when the two dates are equal", () => {
    expect(daysBetween("2026-05-27", "2026-05-27")).toBe(0);
  });

  it("returns negative when toIso is before fromIso", () => {
    expect(daysBetween("2026-06-04", "2026-05-27")).toBe(-8);
  });

  it("handles month boundaries", () => {
    expect(daysBetween("2026-05-30", "2026-06-02")).toBe(3);
  });

  it("handles year boundaries", () => {
    expect(daysBetween("2025-12-25", "2026-01-04")).toBe(10);
  });

  it("handles leap year correctly", () => {
    expect(daysBetween("2024-02-28", "2024-03-01")).toBe(2); // 28 → 29 → 01
    expect(daysBetween("2025-02-28", "2025-03-01")).toBe(1); // 28 → 01 (no leap)
  });

  it("returns null on malformed input", () => {
    expect(daysBetween("not-a-date", "2026-05-27")).toBeNull();
    expect(daysBetween("2026-05-27", "")).toBeNull();
  });
});

describe("addDays ↔ daysBetween round-trip", () => {
  // This is the load-bearing invariant for the payment-deadline UX —
  // the offset-days input and the absolute-date input must round-trip
  // through each other so the operator sees a consistent value when
  // editing either field.
  it("addDays(from, daysBetween(from, to)) === to (both directions)", () => {
    const pairs: [string, string][] = [
      ["2026-05-27", "2026-06-04"],
      ["2026-12-25", "2027-01-04"],
      ["2024-02-28", "2024-03-15"],
      ["2026-05-27", "2026-05-27"], // zero offset
      ["2026-06-04", "2026-05-27"], // negative offset
    ];
    for (const [from, to] of pairs) {
      const n = daysBetween(from, to);
      expect(n).not.toBeNull();
      expect(addDays(from, n!)).toBe(to);
    }
  });

  it("daysBetween(from, addDays(from, n)) === n for arbitrary n", () => {
    const from = "2026-05-27";
    for (const n of [0, 1, 8, 30, 31, 365, -1, -30, -365]) {
      const to = addDays(from, n);
      expect(to).not.toBeNull();
      expect(daysBetween(from, to!)).toBe(n);
    }
  });
});

describe("comfortZone", () => {
  const invoice = "2026-05-27";
  const deadline = "2026-06-04"; // invoice + 8 days

  it("InRange when delivery == invoice (left endpoint, inclusive)", () => {
    expect(comfortZone(invoice, deadline, "2026-05-27")).toBe("InRange");
  });

  it("InRange when delivery == payment_deadline (right endpoint, inclusive)", () => {
    expect(comfortZone(invoice, deadline, "2026-06-04")).toBe("InRange");
  });

  it("InRange when delivery is strictly between the two endpoints", () => {
    expect(comfortZone(invoice, deadline, "2026-05-30")).toBe("InRange");
  });

  it("BeforeInvoiceDate when delivery is the day before invoice", () => {
    expect(comfortZone(invoice, deadline, "2026-05-26")).toBe("BeforeInvoiceDate");
  });

  it("BeforeInvoiceDate when delivery is months earlier (legitimate backdating)", () => {
    expect(comfortZone(invoice, deadline, "2026-01-10")).toBe("BeforeInvoiceDate");
  });

  it("AfterPaymentDeadline when delivery is the day after deadline", () => {
    expect(comfortZone(invoice, deadline, "2026-06-05")).toBe("AfterPaymentDeadline");
  });

  it("AfterPaymentDeadline when delivery is far in the future", () => {
    expect(comfortZone(invoice, deadline, "2027-01-01")).toBe("AfterPaymentDeadline");
  });

  it("InRange even when invoice_date == payment_deadline (zero-day range, single-day comfort zone)", () => {
    // Cash sales: invoice issued + due same day. The point-interval
    // [d, d] is still inclusive, so delivery == that day is in range.
    expect(comfortZone("2026-05-27", "2026-05-27", "2026-05-27")).toBe("InRange");
    expect(comfortZone("2026-05-27", "2026-05-27", "2026-05-26")).toBe("BeforeInvoiceDate");
    expect(comfortZone("2026-05-27", "2026-05-27", "2026-05-28")).toBe("AfterPaymentDeadline");
  });

  it("returns null when payment_deadline is before invoice_date (malformed range)", () => {
    // A malformed range means the operator's form has a deeper
    // problem (deadline typed before invoice date); refuse to
    // classify rather than producing a confusing answer.
    expect(comfortZone("2026-06-04", "2026-05-27", "2026-05-30")).toBeNull();
  });

  it("returns null on any malformed input", () => {
    expect(comfortZone("not-a-date", deadline, "2026-05-30")).toBeNull();
    expect(comfortZone(invoice, "not-a-date", "2026-05-30")).toBeNull();
    expect(comfortZone(invoice, deadline, "not-a-date")).toBeNull();
  });
});

describe("overrideKindForZone", () => {
  it("maps InRange → null (no audit override)", () => {
    expect(overrideKindForZone("InRange")).toBeNull();
  });

  it("maps BeforeInvoiceDate → BeforeInvoiceDate (audit override)", () => {
    expect(overrideKindForZone("BeforeInvoiceDate")).toBe("BeforeInvoiceDate");
  });

  it("maps AfterPaymentDeadline → AfterPaymentDeadline (audit override)", () => {
    expect(overrideKindForZone("AfterPaymentDeadline")).toBe("AfterPaymentDeadline");
  });
});

describe("todayLocalIso", () => {
  // Local-clock convenience for the initial render; the server's
  // stamp is the true issue date. Pin shape only — the actual value
  // depends on the test runner's clock.
  it("returns a well-formed YYYY-MM-DD string", () => {
    const today = todayLocalIso();
    expect(today).toMatch(/^\d{4}-\d{2}-\d{2}$/);
    expect(parseIsoDate(today)).not.toBeNull();
  });

  it("respects an injected `now` argument (for deterministic tests)", () => {
    // Local-timezone-sensitive: construct the expected value the same
    // way the helper does, from a fixed instant.
    const fixed = new Date(2026, 4, 27, 12, 0, 0); // May = month 4
    expect(todayLocalIso(fixed)).toBe("2026-05-27");
  });
});
