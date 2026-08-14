import { describe, expect, it } from "vitest";
// Vite's `?raw` query — the component source as a string. This package
// mounts no components, so the shell's contract is pinned by reading its
// source. Honest scope — these cannot prove the banner RENDERS or is
// legible; they catch the regressions with a plausible motive, listed
// per-test below.
import page from "../routes/StatisticsPage.svelte?raw";
import api from "./api.ts?raw";

// The backend's audit-ledger walk used to swallow any entry whose payload it
// could not decode, so a malformed payment / ack / storno silently made the
// Financial dashboard's figures wrong. It now counts them onto
// `FinancialReport.ledger_diagnostics`. These pins hold the SPA's half of
// that contract: the count must reach the operator, above the numbers it
// invalidates, and must not be silenceable.

/** The integrity banner block only — its `{#if}` guard through the closing
 * `</div>`. Sliced on the `</div>` rather than the first `{/if}`, which
 * belongs to the nested "and N more" arm. */
const banner = (() => {
  const start = page.indexOf("{#if r.ledger_diagnostics.unparseable_entries > 0}");
  expect(start, "the integrity banner's {#if} guard must exist").toBeGreaterThan(-1);
  const end = page.indexOf("</div>", start);
  expect(end, "the integrity banner must close its element").toBeGreaterThan(start);
  return page.slice(start, end);
})();

describe("StatisticsPage ledger-integrity banner", () => {
  it("is announced as an alert", () => {
    // Drop `role="alert"` and a screen-reader operator is told nothing at all
    // — the figures just quietly stay wrong, which is the original defect
    // wearing a different hat.
    expect(banner).toContain('role="alert"');
  });

  it("fires ONLY on a non-zero count, so a healthy ledger shows nothing", () => {
    // The inverse failure mode: a banner that cries wolf on every report
    // teaches the operator to ignore it, and then the one real occurrence is
    // invisible too.
    expect(page).toContain("{#if r.ledger_diagnostics.unparseable_entries > 0}");
  });

  it("says the figures may be INCOMPLETE, not merely that something failed", () => {
    // "N records could not be read" alone reads as a cosmetic glitch. The
    // operator needs to know it invalidates the numbers on screen.
    expect(banner).toMatch(/could not be read/i);
    expect(banner).toMatch(/incomplete/i);
  });

  it("names the offending audit entries so they can be found", () => {
    // A bare count is not actionable; the ids are the operator's (and
    // Ervin's) starting point in the ledger.
    expect(banner).toContain("r.ledger_diagnostics.unparseable_entry_ids");
  });

  it("discloses the backend id cap instead of implying the list is complete", () => {
    // The backend caps the id list at 50 but keeps the count exact. If the
    // SPA rendered only the ids, a 500-entry corruption would read as 50.
    expect(banner).toContain(
      "r.ledger_diagnostics.unparseable_entries > r.ledger_diagnostics.unparseable_entry_ids.length",
    );
  });

  it("has NO dismiss control", () => {
    // Nothing here may let the operator click away a data-integrity alarm;
    // it goes down when the backend stops reporting it.
    expect(banner).not.toMatch(/<button/i);
    expect(banner).not.toMatch(/dismiss|onclick/i);
  });

  it("sits ABOVE the figures, not in the collapsed deferred-notes disclosure", () => {
    // `deferred_notes` is a `<details>` labelled "Deferred to a later
    // release" — the wrong home for "today's numbers may be wrong". Folding
    // this in there would technically surface it and practically bury it.
    const bannerAt = page.indexOf("{#if r.ledger_diagnostics.unparseable_entries > 0}");
    const cardsAt = page.indexOf('<section class="stats__cards"');
    const deferredAt = page.indexOf('<details class="stats__deferred">');
    expect(cardsAt).toBeGreaterThan(-1);
    expect(deferredAt).toBeGreaterThan(-1);
    expect(bannerAt).toBeLessThan(cardsAt);
    expect(bannerAt).toBeLessThan(deferredAt);
  });
});

// The aging panels had the SAME silent drop on a different code path: an
// outstanding invoice whose `payment_deadline` was missing or unreadable
// fell out of every bucket while still counting toward the receivables /
// payables total, so the buckets summed to less than the headline above
// them. The backend now ages it as 90+ and counts it. The bucket is
// therefore an IMPUTATION and must not read as a measurement.
//
// It is disclosed QUIETLY, and that is a considered choice rather than an
// oversight: `ap_sync` records no payment deadline at all for NAV-synced
// payables, so on a real book this condition is permanent and universal.
// A page-level alert would be lit on every single load and a rendered id
// list would be a permanent wall of ids — both teach the operator to
// ignore the page, which is how the ledger-integrity banner ABOVE (a rare
// and genuinely alarming signal) loses its meaning too. So: a per-side
// count, inline under the panel it qualifies, in the same muted
// `stats__detail` chrome as the existing "counts are exact" footnote. The
// ids stay on the wire for support.

/** The aging-panel snippet — the footnote's home. Sliced to the snippet
 * body so a page-level block elsewhere cannot satisfy these by accident. */
const agingSnippet = (() => {
  const start = page.indexOf("{#snippet agingPanel(");
  expect(start, "the aging-panel snippet must exist").toBeGreaterThan(-1);
  const end = page.indexOf("{/snippet}", start);
  expect(end, "the aging-panel snippet must close").toBeGreaterThan(start);
  return page.slice(start, end);
})();

describe("StatisticsPage undated-aging footnote", () => {
  it("discloses the count inline, in the panel it qualifies", () => {
    // Saying nothing is the failure this pin blocks: an imputed 90+ that
    // reads as measured is the original defect wearing a nicer hat.
    expect(agingSnippet).toContain("undatedCount");
    expect(agingSnippet).toMatch(/\{#if undatedCount > 0\}/);
    expect(agingSnippet).toMatch(/no recorded due date/i);
    expect(agingSnippet).toMatch(/90\+/);
  });

  it("is fed the PER-SIDE counts, not the combined total", () => {
    // The combined figure under both panels would double-report it: a
    // payables-only problem would show up as receivables trouble too.
    expect(page).toContain("r.ledger_diagnostics.aging_undated_receivables");
    expect(page).toContain("r.ledger_diagnostics.aging_undated_payables");
  });

  it("stays quiet: no alert role, no alarm chrome, no dismiss control", () => {
    // The distinction this pin holds against drift back to a banner: the
    // figures are COMPLETE, one column of them is an estimate. Escalating
    // that to role="alert" next to the real integrity banner devalues
    // both.
    expect(agingSnippet).not.toMatch(/role="(alert|status)"/);
    expect(agingSnippet).not.toContain("stats__integrity");
    expect(agingSnippet).not.toMatch(/dismiss/i);
  });

  it("renders NO id list anywhere on the dashboard", () => {
    // The whole point of the quiet form. `aging_undated_invoice_ids` is
    // machine-readable diagnostics; rendering it puts 50 ids permanently
    // on Ervin's screen.
    expect(page).not.toContain("aging_undated_invoice_ids");
  });

  it("drives no page-level block of its own", () => {
    // The undated counters must reach the operator ONLY as snippet
    // arguments feeding the inline footnote. A `{#if ... > 0}` block at
    // page level is the drift back to the alarm form this replaced —
    // whatever chrome it wears.
    expect(page).not.toMatch(/\{#if r\.ledger_diagnostics\.aging_undated/);
    for (const m of page.matchAll(/r\.ledger_diagnostics\.aging_undated_\w+/g)) {
      const line = page.slice(page.lastIndexOf("\n", m.index) + 1, page.indexOf("\n", m.index));
      expect(line.trim(), "undated counts belong in the agingPanel call, nowhere else").toMatch(
        /^r\.ledger_diagnostics\.aging_undated_(receivables|payables),$/,
      );
    }
  });
});

describe("FinancialReport wire shape", () => {
  it("carries ledger_diagnostics as a required field", () => {
    // Optional (`?:`) would let the banner's guard silently short-circuit on
    // every report if the backend field were ever dropped — failing back to
    // exactly the silence this fix removed.
    expect(api).toMatch(/ledger_diagnostics:\s*LedgerDiagnostics;/);
    expect(api).toMatch(/unparseable_entries:\s*number;/);
    expect(api).toMatch(/unparseable_entry_ids:\s*string\[\];/);
  });

  it("carries the undated-aging counters as required fields too", () => {
    expect(api).toMatch(/aging_undated_invoices:\s*number;/);
    expect(api).toMatch(/aging_undated_receivables:\s*number;/);
    expect(api).toMatch(/aging_undated_payables:\s*number;/);
  });

  it("keeps the id list on the WIRE even though the page does not render it", () => {
    // Unrendered is not unreported. Dropping the field would take away
    // support's only way to find the offending invoices, since the log is
    // the debug-level per-invoice line.
    expect(api).toMatch(/aging_undated_invoice_ids:\s*string\[\];/);
  });
});
