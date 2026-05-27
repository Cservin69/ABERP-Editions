# ADR-0044 — Printed-Invoice Palette: Silver Structure, One Gold Accent

**Status:** Accepted — PR-85 (2026-05-27)
**Author:** Ervin Áben (ABERP), session 108 (visual brief)
**Supersedes / amends:** none (additive — `crates/invoice-pdf` was monochrome grey pre-PR-85)
**Related:** ADR-0021 (print rendering path posture), ADR-0037 (EUR-invoicing printed-fields scope), the `reference_aberp_invoice_template.md` reference + its "PDF polish feedback" section (the brief that drove this PR)

## Context

ABERP's `crates/invoice-pdf` renderer ships the document Áben
Consulting KFT.'s real clients receive. Pre-PR-85 the renderer used a
single hard-coded grey (`Object::Real(0.7)` literals scattered through
`rule()`) for every structural line and no fill-colour on text, so
the PDF rendered as monochrome ink-on-white — functionally correct,
visually generic. With go-live for real invoicing ~2026-06-10, the
"looks like a dev tool" baseline stopped being acceptable.

Ervin's brief (PR-85): _"Posh it up — premium palette: silver, with
minimal gold accents. I trust your taste."_ — together with three
concrete cramping fixes (label/value spacing, table gutters, long-
description wrap).

Two genuine forces in tension:

1. **Refined-luxury brand feel.** The competitive reference (Billingo's
   generated invoices) uses a single red/orange accent against grey
   structural rules to read as "professional, not flashy". ABERP's
   client-facing surface needs the same posture — restraint over
   ornamentation — but with a palette that reads as ABERP-the-product
   rather than borrowing Billingo's accent.
2. **Regulatory legibility.** The HU §169/§80(1)(g) printed surface is
   accountant-read; over-styling (heavy borders, multiple colours,
   coloured numerics) makes the page feel like a marketing brochure
   and degrades the accountant's read-time. The colour scheme must
   never undermine the data hierarchy: number > label > rule.

## Decision

Three named-colour constants live at `crates/invoice-pdf/src/lib.rs`,
governed by three discipline rules.

### Palette

| Token | RGB (0–1) | Used for |
| --- | --- | --- |
| `INK` | `(0.13, 0.13, 0.15)` | Primary text — names, amounts, dates, invoice number, headline `Számla`. NOT pure black; a faint warm shift so ink pairs with the silver/gold accents instead of fighting them. |
| `MUTED` | `(0.46, 0.47, 0.51)` | Section labels (`ELADÓ`, `VEVŐ`, `ADÓSZÁM:`, `NETTÓ ÖSSZEG:`, `MEGJEGYZÉS`), table column headers, footer attestation. Refined silver-grey — sits below ink in the hierarchy without disappearing. |
| `SILVER_LINE` | `(0.72, 0.72, 0.74)` | Structural rules — title under-rule, table header rule, table footer rule. Soft warm silver: visible, never competing with the ink content above/below. Stroke weight 0.5pt. |
| `GOLD_ACCENT` | `(0.72, 0.54, 0.12)` | The ONE accent in the document — the rule above the totals banner. Stroke weight 0.85pt (slightly heavier than silver so the accent reads as deliberate). |

### Three discipline rules

1. **Structural rules in `SILVER_LINE`.** Every rule the renderer emits
   goes through `silver_rule(...)` unless it is the totals-banner rule.
2. **ONE gold accent.** The rule above `FIZETENDŐ BRUTTÓ VÉGÖSSZEG` is
   the only `gold_rule(...)` call in the renderer. The big total
   figure stays `INK` bold — sparing, not gaudy. Adding gold to a
   second element (e.g., the total figure itself, the invoice number,
   the title rule) is a brand drift and should fail review.
3. **Section labels in `MUTED`.** Small-caps feel comes from existing
   uppercase strings + the smaller font size (7-8pt), not from extra
   typography ops (no `Tc` character-spacing tricks — kept tasteful
   AND WinAnsi-safe).

### Spacing fixes (visual brief items 2–5)

These are not palette decisions per se but they ship in the same PR
because they were the same brief — and they are pinned by tests in
the same module so they stay coherent with the palette.

- `LABEL_VALUE_GAP = 10pt` (was 4pt). Used by `label_value()` to space
  any "LABEL: value" pair (ADÓSZÁM, IBAN, BANKSZÁMLASZÁM, BANK NEVE,
  SWIFT/BIC, SZÁMLA KELTE, TELJESÍTÉS KELTE, FIZETÉSI HATÁRIDŐ,
  FIZETÉSI MÓD). The pre-PR-85 4pt gap rendered as `Adószám:123` —
  visually merged.
- Line-item column right-edges retuned to widen inter-column gutters
  and pull the rightmost column 6pt off `MARGIN_RIGHT` (Ervin: "indent
  columns a bit left" + "add column padding"). The retune also fixes
  a separate cramping bug surfaced by the visual brief: the pre-PR-85
  `VAT_RIGHT` placement collided with `GROSS_RIGHT`-anchored data
  because the renderer's `0.55 × size` per-char width proxy under-
  estimates `%` and uppercase header glyphs by 5-10pt.
- `DESC_WRAP_CHARS = 40` enables long product-description wrap. Lines
  exceed wrap width and flow to additional rows with row-height
  growing by 11pt per extra line — Ervin's "let it flow onto additional
  lines and grow the row height".

## Consequences

### Wins

- A future brand iteration is a one-line edit per token — palette is
  tunable in one place, not grep-and-replace across thirty `0.7` greys.
- The single-accent posture is enforced structurally: there are exactly
  two rule-emitting functions (`silver_rule` / `gold_rule`), and a
  test pins the four palette RGB triples so a "let me just nudge the
  gold a bit" drift trips at test-time.
- Per-line description wrap is the load-bearing flex point for long
  product names — the renderer no longer truncates or squeezes; long
  rows simply grow taller, with the totals block automatically
  flowing down.
- The HUF + EUR sample PDFs Ervin can eyeball ship inside the crate
  (`crates/invoice-pdf/examples/render_samples.rs`) so any future
  visual regression is a `cargo run --example render_samples` + sips
  away from the brand decision.

### Trade-offs

- The renderer still uses `Helvetica` + WinAnsi encoding (per the
  PR-44ε.1 / A152 decision). Hungarian double-acute `ő/ű` still
  substitute to single-acute `ö/ü` at the byte boundary. A future
  PR-44ε.2 font-embedding lift unlocks proper Unicode + glyph-width-
  based right-alignment, which would also eliminate the proxy-width
  underestimate problem behind the column-cramping bug this PR
  worked around structurally.
- "ONE gold accent" is a discipline rule, not a structural one — the
  codebase doesn't prevent a future contributor from calling
  `gold_rule` somewhere else. The test pins the palette VALUES, not
  the call-count. If gold creep becomes a problem, a `static_assertions`
  pin on the `gold_rule` call sites can be added at PR-85+1.
- The `0.55 × size` per-char proxy in `text_right` is unchanged. The
  column-cramping fix is layout-side (give VAT enough breathing room
  that even the underestimate doesn't cause overlap). A future PR
  that revisits the proxy would simplify this.

### Reversibility

Trivially reversible — the palette is four `const Color = (f32, f32,
f32);` lines. Setting them all to `(0.0, 0.0, 0.0)` plus pointing
both rule functions at silver returns to the pre-PR-85 monochrome
look (modulo the spacing + wrap changes, which are independent
improvements and stay).

## Alternatives considered

- **Mono-grey, no accent.** Considered + rejected: the live render
  Ervin saw mid-session was already mono-grey and he explicitly
  asked for the polish pass. The accent is the differentiator
  between "looks dev-tool" and "looks like a real invoice".
- **Two-accent (silver structure + gold rule + coloured total
  figure).** Considered + rejected as gaudy. The big bold total
  figure already carries enough visual weight via size (20pt vs 9pt
  surroundings) + the gold rule directly above it. Adding gold to
  the figure itself reads as decorative, not deliberate.
- **Embed a custom display font (e.g., Inter) for the title block.**
  Considered + rejected for this PR: shipping a font file inflates
  the binary, requires a Type0/CIDFontType2 dictionary, and overlaps
  with the deferred PR-44ε.2 Hungarian-double-acute lift. If
  PR-44ε.2 embeds a Unicode font anyway, the typography upgrade
  rides on top of that PR's foundation. Out of scope here.
- **CSS-style per-cell padding via a layout engine.** Considered +
  rejected: introducing a layout library to handle "5 extra points of
  column padding" is a CLAUDE.md rule 2 violation. The existing
  absolute-`Td`-positioning model is fine; the inputs to it are what
  needed tuning.

## Invariants

- A regression that removes any of the four palette constants OR
  drops their values to the pre-PR-85 monochrome posture trips the
  `palette_constants_match_brand_decision` test in
  `crates/invoice-pdf/src/lib.rs`.
- A regression that reverts `LABEL_VALUE_GAP` below 8pt (the brand
  decision is 10pt; 8pt is the test floor — enough headroom for a
  small tweak, but the pre-PR-85 4pt regression trips loudly) fails
  the `label_value_gap_breathes` test.
- A regression that disables description wrapping fails the
  `description_wraps_when_long` test.
- All four pre-PR-85 `print_invoice_render` integration tests
  (`eur_invoice_renders_with_arfolyam_and_huf_totals`,
  `huf_invoice_renders_without_arfolyam_line`,
  `eur_invoice_uses_round_half_even_for_per_rate_huf`,
  `printed_invoice_pdf_is_single_page`) still pass, pinning that
  the visual pass did not regress any §80(1)(g) regulatory content.
- The PR-82 notes path + the PR-84 three-dates path both ride
  through unchanged — palette tokens replace literal `0.7`s, but
  the data flow into the renderer is byte-identical.
