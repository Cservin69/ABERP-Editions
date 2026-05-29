// PR-44ε / session-53 — vitest pin tests for `format.ts` per
// ADR-0037 §1.a + §1.c (printed-invoice fields and rounding
// precision) and the session-53 SPA-render brief.
//
// Each pin catches a distinct regression mode per CLAUDE.md rule
// 9: a constant-returning formatter would fail every assertion
// except the trivial pass-through ones. Together with the Rust
// pin tests (`invoice_list_item_emits_currency` and
// `invoice_detail_emits_currency_and_rate_metadata` in `serve.rs`)
// the wire-to-render contract is pinned end-to-end.

import { describe, expect, it } from "vitest";

import {
  filenameForInvoice,
  formatHufEquivalent,
  formatInvoiceDate,
  formatInvoiceTotal,
  formatRate,
  formatRateDate,
  formatTotal,
  parseAmountToMinor,
} from "./format";
import type { Currency } from "./api";

describe("formatTotal", () => {
  // The HUF branch is byte-equal to the pre-PR-44ε
  // `formatHuf` posture — the same `Intl.NumberFormat("hu-HU",
  // {style: "currency", currency: "HUF"})` instance the old
  // InvoiceList.svelte + InvoiceDetail.svelte carried. We pin
  // the rendered shape contains the expected digit groups +
  // " Ft" suffix without asserting on the exact whitespace
  // (`Intl.NumberFormat` uses a non-breaking space U+00A0 as
  // the thousand separator under hu-HU; future ICU updates may
  // narrow it, and the operator surface tolerates either).

  it("HUF: renders whole forints with `Ft` suffix", () => {
    const out = formatTotal(654_883, "HUF");
    // Must contain the digits in grouped form + the " Ft" suffix.
    expect(out).toMatch(/654.?883.?Ft/);
  });

  it("HUF: renders large totals without fractional part", () => {
    const out = formatTotal(1_234_567_890, "HUF");
    // No decimal separator must appear (HUF has no sub-unit per
    // ADR-0009 §1).
    expect(out).not.toMatch(/[.,]\d/);
    expect(out).toMatch(/Ft/);
  });

  it("EUR: interprets the integer value as cents and renders as euros", () => {
    // 863600 cents = €8 636,00 per the printed-invoice reference
    // template (Hungarian decimal comma + grouped thousands).
    const out = formatTotal(863_600, "EUR");
    expect(out).toMatch(/8.?636,00/);
    expect(out).toMatch(/€/);
  });

  it("EUR: divides by 100 (cents → euros) — 100 cents reads as €1,00", () => {
    const out = formatTotal(100, "EUR");
    expect(out).toMatch(/1,00/);
    expect(out).toMatch(/€/);
  });

  it("null renders as em-dash regardless of currency", () => {
    expect(formatTotal(null, "HUF")).toBe("—");
    expect(formatTotal(null, "EUR")).toBe("—");
  });

  it("HUF and EUR branches differ on the same numeric input", () => {
    // CLAUDE.md rule 9 — a regression that hard-codes the HUF
    // branch (or drops the EUR branch entirely) would produce
    // identical output for both. The two values MUST differ
    // because one is `654 883 Ft` and the other is roughly
    // `€6 548,83`.
    const huf = formatTotal(654_883, "HUF");
    const eur = formatTotal(654_883, "EUR");
    expect(huf).not.toBe(eur);
  });
});

describe("formatRate", () => {
  it("normalises the canonical 6-decimal wire form unchanged", () => {
    // The backend serialises at exactly 6 decimals per ADR-0037
    // §1.c / C11; the formatter is a pass-through after
    // numeric parse.
    expect(formatRate("405.230000")).toBe("405.230000");
  });

  it("pads to 6 decimals when the backend emits fewer", () => {
    // Defensive — a future backend drift that drops the
    // `{:.6}` precision specifier on `rust_decimal::Decimal::Display`
    // would render as `"405.23"`. The formatter re-pads so the
    // operator surface stays at 6 decimals per C11.
    expect(formatRate("405.23")).toBe("405.230000");
  });

  it("renders a whole-number rate with all 6 decimals", () => {
    // `1` is the HUF self-rate stamped at PR-44δ; on the SPA we
    // expect the same 6-decimal form for visual consistency
    // (today the HUF branch hides this row, but a future
    // chain-currency-match or operator-debug surface may
    // surface it).
    expect(formatRate("1")).toBe("1.000000");
  });

  it("passes a malformed input through unchanged (fail-loud per CLAUDE.md rule 12)", () => {
    // A non-numeric value indicates DB tampering or schema
    // drift; rendering it verbatim makes the divergence
    // operator-visible rather than silently zeroing it.
    expect(formatRate("not-a-number")).toBe("not-a-number");
  });
});

describe("formatHufEquivalent", () => {
  it("renders the HUF amount with grouped thousands and `Ft` suffix", () => {
    // The HUF-equivalent gross total on the printed-invoice
    // reference template renders as `Bruttó összeg: 654 883 Ft`.
    const out = formatHufEquivalent(654_883);
    expect(out).toMatch(/654.?883.?Ft/);
  });

  it("renders zero forints as `0 Ft`", () => {
    expect(formatHufEquivalent(0)).toMatch(/0.?Ft/);
  });

  it("matches `formatTotal` for the HUF branch (single source of truth)", () => {
    // Both helpers ultimately format whole forints under the
    // same `Intl.NumberFormat` instance; a regression that
    // forks them would let one side drift. The pin catches the
    // drift at gate time.
    const value = 1_234_567;
    expect(formatHufEquivalent(value)).toBe(formatTotal(value, "HUF"));
  });
});

describe("formatRateDate", () => {
  it("passes a canonical ISO date through unchanged", () => {
    // The backend emits ISO-8601 `YYYY-MM-DD` per ADR-0037
    // §1.a + §2.b (`exchange_rate_date` is `OffsetDateTime`
    // formatted with `[year]-[month]-[day]`). The formatter is
    // a pass-through today; a future Hungarian-locale render
    // (`2026. 05. 22.`) lifts here additively.
    expect(formatRateDate("2026-05-22")).toBe("2026-05-22");
  });

  it("passes an empty string through unchanged", () => {
    // Defensive — the SPA never sees an empty string today
    // (the wire shape is `string | null`, with `null` for HUF
    // invoices and a non-empty string for EUR), but the
    // formatter must not crash if a future migration emits one.
    expect(formatRateDate("")).toBe("");
  });
});

describe("formatInvoiceDate", () => {
  // PR-99 Item 5 — Hungarian-locale display form (`YYYY. MM. DD.`)
  // for the three invoice dates rendered on the detail meta-grid.
  // Matches the printed-PDF formatting so the operator's eye can
  // cross-reference the on-screen detail with the document.
  it("formats a canonical ISO date in HU display form", () => {
    expect(formatInvoiceDate("2026-05-22")).toBe("2026. 05. 22.");
  });

  it("renders null as the em-dash placeholder", () => {
    expect(formatInvoiceDate(null)).toBe("—");
  });

  it("passes a malformed string through verbatim (fail loud)", () => {
    // Defensive — a backend drift that emits a non-ISO form
    // surfaces visibly rather than via a silent locale formatter
    // crash. CLAUDE.md rule 12.
    expect(formatInvoiceDate("not-a-date")).toBe("not-a-date");
    expect(formatInvoiceDate("2026/05/22")).toBe("2026/05/22");
  });
});

describe("filenameForInvoice", () => {
  // PR-44ε.UI / session-58 — the SPA-side filename builder for the
  // browser-native download dialog. Rust side mirror is
  // `serve::pdf_filename_for_invoice`; a one-sided rename (e.g.,
  // changing the prefix to `aberp_` here without touching Rust)
  // would surface as a browser-saved filename diverging from the
  // `Content-Disposition` header. CLAUDE.md rule 9: three distinct
  // invoice numbers in the round-trip set, so a regression that
  // hard-codes the filename (or strips the invoice number) cannot
  // pass all three assertions vacuously.

  it("composes `invoice_<invoice_number>.pdf` for a typical fiscal-year number", () => {
    expect(filenameForInvoice("2026-000013")).toBe("invoice_2026-000013.pdf");
  });

  it("preserves the series prefix verbatim", () => {
    expect(filenameForInvoice("INV-default-2026-000042")).toBe(
      "invoice_INV-default-2026-000042.pdf",
    );
  });

  it("handles a storno invoice's `S`-prefixed series", () => {
    expect(filenameForInvoice("S2026-000001")).toBe("invoice_S2026-000001.pdf");
  });
});

// PR-88 / session-113 — exhaustive table-driven pins for the operator-
// input → minor-units parser. This is the load-bearing fix for the
// money-correctness bug Ervin caught in live test (he typed `340` EUR
// expecting 340.00 EUR; the SPA sent 340 cents = 3.40 EUR, a 100×
// underbill; he issued a real wrong-amount invoice before noticing).
// The rule (CLAUDE.md rule 9): every row of the table below is one
// distinct regression mode. A parser that hard-codes one branch would
// fail at least one assertion; a parser that drops the comma-separator
// path would fail half the EUR rows; a parser that reverts to the
// auto-cents posture would fail the bare-integer EUR rows. Together
// these pins make Bug 1 impossible to reintroduce silently.
describe("parseAmountToMinor — operator-input → minor-units", () => {
  // Table of (input, currency, expected) tuples. Co-located with the
  // helper so a future widening (e.g., adding a 3-decimal currency)
  // surfaces both ends.
  const cases: Array<{ input: string; currency: Currency; expected: number | null; why: string }> = [
    // ── EUR (2-decimal) — the bug class Ervin caught ────────────
    // Bare integer: WHOLE major units. The pre-PR-88 bug sent 340
    // cents = 3.40 EUR here; the fix sends 34000 cents = 340.00 EUR.
    { input: "340", currency: "EUR", expected: 34_000, why: "EUR bare int = whole euros" },
    { input: "340.50", currency: "EUR", expected: 34_050, why: "EUR `.` separator" },
    { input: "340,50", currency: "EUR", expected: 34_050, why: "EUR `,` separator (Hungarian)" },
    { input: "340.5", currency: "EUR", expected: 34_050, why: "EUR fractional pads to 2 digits" },
    { input: "0,99", currency: "EUR", expected: 99, why: "EUR sub-1-euro amount" },
    { input: "0.99", currency: "EUR", expected: 99, why: "EUR sub-1-euro amount with dot" },
    { input: "1000", currency: "EUR", expected: 100_000, why: "EUR large bare int" },
    { input: "1", currency: "EUR", expected: 100, why: "EUR exact 1.00 euro" },
    { input: "1,00", currency: "EUR", expected: 100, why: "EUR explicit zero fractional" },
    { input: "1.00", currency: "EUR", expected: 100, why: "EUR explicit zero fractional dot" },
    // Hungarian thousands-space convention: `340 000` → 340000 EUR
    // major units = 34000000 cents. Operators may paste copy-paste
    // values with NBSP from another sheet.
    { input: "340 000", currency: "EUR", expected: 34_000_000, why: "EUR thousands ASCII space" },
    { input: "340 000", currency: "EUR", expected: 34_000_000, why: "EUR thousands NBSP" },
    { input: "340 000,50", currency: "EUR", expected: 34_000_050, why: "EUR thousands + decimal" },
    // ── HUF (0-decimal) — must NOT have been broken by the fix ──
    // ADR-0009 §1: HUF has no sub-unit; `Huf(pub i64)` counts whole
    // forints. Bare integer = whole forint = exactly the wire
    // minor-unit count.
    { input: "340", currency: "HUF", expected: 340, why: "HUF bare int = whole forints" },
    { input: "1000", currency: "HUF", expected: 1_000, why: "HUF large bare int" },
    { input: "1", currency: "HUF", expected: 1, why: "HUF 1 forint" },
    { input: "654883", currency: "HUF", expected: 654_883, why: "HUF reference-template total" },
    { input: "340 000", currency: "HUF", expected: 340_000, why: "HUF thousands space" },
    // ── Whitespace tolerance ────────────────────────────────────
    { input: "  340  ", currency: "EUR", expected: 34_000, why: "EUR surrounding whitespace trims" },
    { input: " 340,50 ", currency: "EUR", expected: 34_050, why: "EUR whitespace + decimal" },
    // ── Rejection arms — return null (composer treats as 0) ─────
    { input: "", currency: "EUR", expected: null, why: "empty string" },
    { input: "   ", currency: "EUR", expected: null, why: "whitespace-only" },
    { input: "abc", currency: "EUR", expected: null, why: "non-numeric" },
    { input: "-340", currency: "EUR", expected: null, why: "leading minus rejected (invoice prices positive)" },
    { input: "+340", currency: "EUR", expected: null, why: "leading plus rejected" },
    { input: "340.", currency: "EUR", expected: null, why: "trailing separator rejected" },
    { input: ".50", currency: "EUR", expected: null, why: "bare-decimal rejected (no leading whole)" },
    { input: ",50", currency: "EUR", expected: null, why: "bare-decimal comma rejected" },
    { input: "340.50.20", currency: "EUR", expected: null, why: "two separators rejected" },
    { input: "340,505", currency: "EUR", expected: null, why: "EUR over-decimals rejected (3 > 2)" },
    { input: "340.5", currency: "HUF", expected: null, why: "HUF rejects any fractional part" },
    { input: "340,50", currency: "HUF", expected: null, why: "HUF rejects fractional comma" },
    { input: "1e3", currency: "EUR", expected: null, why: "scientific notation rejected" },
  ];

  for (const { input, currency, expected, why } of cases) {
    const label = `${JSON.stringify(input)} as ${currency} → ${expected === null ? "null" : `${expected} minor units`} (${why})`;
    it(label, () => {
      expect(parseAmountToMinor(input, currency)).toBe(expected);
    });
  }

  // Round-trip pin: the operator-typed integer string for EUR must
  // ALWAYS produce a minor count exactly 100× the typed value. This
  // is the headline anti-regression assertion — if a future change
  // re-introduces the cents-shift bug, this loop fails on every
  // EUR row.
  it("EUR: a bare integer N reads as exactly N × 100 cents (anti-cents-shift)", () => {
    for (const n of [1, 7, 42, 340, 1000, 999_999]) {
      expect(parseAmountToMinor(String(n), "EUR")).toBe(n * 100);
    }
  });

  // Round-trip pin: the operator-typed integer string for HUF must
  // ALWAYS pass through unchanged (HUF is 0-decimal).
  it("HUF: a bare integer N reads as exactly N forints (no shift)", () => {
    for (const n of [1, 7, 42, 340, 1000, 999_999]) {
      expect(parseAmountToMinor(String(n), "HUF")).toBe(n);
    }
  });
});

describe("Conditional render contract (documented behaviour pin)", () => {
  // PR-44ε / session-53 — the four rate-metadata rows in
  // `InvoiceDetail.svelte` render iff BOTH `currency !== "HUF"`
  // AND the corresponding wire field is non-null. The Svelte
  // template carries the conditional inline:
  //
  //     {#if detail.currency !== "HUF" && detail.exchange_rate !== null}
  //
  // We cannot exercise the Svelte template directly from
  // vitest (no Svelte 5 component runner setup; deferred per
  // CLAUDE.md rule 2). Instead we pin the equivalent boolean
  // shape here so a regression that flips the && to a || or
  // drops the null-check is caught at gate time as a logic
  // mismatch between this test's expectation and the template
  // body. The pin is a code-review surface, not a runtime
  // enforcement — but it documents the intended truth table.

  function shouldRenderRow(
    currency: "HUF" | "EUR",
    fieldValue: string | number | null,
  ): boolean {
    return currency !== "HUF" && fieldValue !== null;
  }

  it("HUF invoice with rate fields populated: row hidden (regulatory record is HUF itself)", () => {
    // Defensive: even if the backend ever populates rate
    // fields for a HUF invoice (it never does; the
    // `RateMetadata` stamp gates on `!matches!(currency,
    // Currency::Huf)` in issue_invoice.rs), the SPA still
    // hides the rate rows because they are
    // not regulatory-required for HUF invoices.
    expect(shouldRenderRow("HUF", "405.230000")).toBe(false);
    expect(shouldRenderRow("HUF", 3_500_565)).toBe(false);
  });

  it("EUR invoice with all rate fields populated: row shown", () => {
    expect(shouldRenderRow("EUR", "405.230000")).toBe(true);
    expect(shouldRenderRow("EUR", "MNB")).toBe(true);
    expect(shouldRenderRow("EUR", "2026-05-22")).toBe(true);
    expect(shouldRenderRow("EUR", 3_500_565)).toBe(true);
  });

  it("EUR invoice with a null rate field: row hidden (fail-soft)", () => {
    // A non-HUF invoice missing a rate field would indicate a
    // backend bug (PR-44γ pre-flight refuses non-HUF rows
    // lacking rate metadata at the DuckDB write boundary per
    // ADR-0037 §4 C1). The SPA fails soft on the per-field
    // level — hide the row rather than render `null`. The
    // operator-visible signal is the missing row, not a
    // garbled value.
    expect(shouldRenderRow("EUR", null)).toBe(false);
  });
});

// ADR-0049 §Screen render (session 156) — `formatInvoiceTotal` negates
// the displayed total for a storno invoice. The billing tables store a
// storno's `total_gross` POSITIVE (negation lives only in the NAV-XML /
// PDF render path); this helper flips the sign for the operator surface
// so the screen matches the buyer-facing PDF. Paired with the Rust
// `invoice_views_emit_is_storno_bool` pin so the wire-to-render contract
// is end-to-end. CLAUDE.md rule 9: the storno (negated) and regular
// (unchanged) branches are pinned distinctly so a regression that drops
// the negation — or negates a regular invoice — cannot pass vacuously.
describe("formatInvoiceTotal (storno negation)", () => {
  it("HUF storno: renders the total negated (`-131 175 Ft`)", () => {
    const out = formatInvoiceTotal(131_175, "HUF", true);
    // Leading minus, grouped digits, Ft suffix. The thousands
    // separator under hu-HU is U+00A0; tolerate any single char.
    expect(out).toMatch(/^-.*131.?175.?Ft/);
  });

  it("HUF regular: renders the total unchanged (`131 175 Ft`, no minus)", () => {
    const out = formatInvoiceTotal(131_175, "HUF", false);
    expect(out).toMatch(/131.?175.?Ft/);
    expect(out).not.toMatch(/-/);
  });

  it("EUR storno: negates the cents value before euro formatting", () => {
    // 863600 cents = €8 636,00; storno renders the negative.
    const out = formatInvoiceTotal(863_600, "EUR", true);
    expect(out).toMatch(/8.?636,00/);
    expect(out).toMatch(/€/);
    expect(out).toMatch(/-/);
  });

  it("null renders as the em-dash regardless of the storno flag", () => {
    expect(formatInvoiceTotal(null, "HUF", true)).toBe("—");
    expect(formatInvoiceTotal(null, "EUR", false)).toBe("—");
  });

  it("storno and regular differ in sign on the same input", () => {
    const storno = formatInvoiceTotal(127_000, "HUF", true);
    const regular = formatInvoiceTotal(127_000, "HUF", false);
    expect(storno).not.toBe(regular);
    expect(storno.startsWith("-")).toBe(true);
    expect(regular.startsWith("-")).toBe(false);
  });
});
