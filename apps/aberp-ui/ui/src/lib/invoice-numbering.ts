// PR-89 / ADR-0045 — pure-data helper for the operator-configurable
// invoice-number template. Mirrors the Rust `apps/aberp/src/numbering.rs`
// surface on the SPA side: segment model + render + validate +
// composer for the wire body. No DOM, no fetch — the routes layer
// (api.ts) handles IO; the TenantSettings page composes UI state.
//
// Pinned by `invoice-numbering.test.ts`.

/** Closed-vocab year-segment width. Matches the Rust
 * `numbering::YearDigits` 1:1; widening requires a deliberate change
 * here + in the Rust enum + in the SPA builder UI. */
export type YearDigits = 2 | 4;

/** Closed-vocab segment kinds. `kind` is the discriminant; the field
 * set varies by kind (mirrors the Rust `Segment` enum via the wire
 * shape `serve.rs::SegmentWire`). */
export type NumberingSegment =
  | { kind: "Literal"; text: string }
  | { kind: "Year"; digits: YearDigits }
  | { kind: "Counter"; pad_width: number };

/** Closed-vocab counter-reset policy. `Never` runs the counter
 * continuously across years (the pre-PR-89 `INV-default` behaviour);
 * `OnYearChange` resets the counter to `start_value` when the
 * calendar year of the issue date changes (Hungarian convention). */
export type NumberingResetPolicy = "never" | "on_year_change";

/** A complete numbering template. Wire-shape match for the Rust
 * `NumberingTemplateWire` — `segments` array + `reset_policy` token +
 * `start_value`. */
export interface NumberingTemplate {
  segments: NumberingSegment[];
  reset_policy: NumberingResetPolicy;
  start_value: number;
}

/** PR-89 — closed vocab of validate-time failure kinds. Mirrors the
 * Rust `NumberingError` variant set (`NoCounter`, `MultipleCounters`,
 * `EmptyLiteral`, `InvalidLiteralCharacter`, `TooLong`,
 * `OnYearChangeWithoutYearSegment`, `InvalidStartValue`,
 * `EmptyTemplate`). The validator returns the first failure encountered
 * — same posture as the Rust validator. */
export type NumberingValidationError =
  | { kind: "EmptyTemplate" }
  | { kind: "NoCounter" }
  | { kind: "MultipleCounters"; count: number }
  | { kind: "EmptyLiteral"; segmentIndex: number }
  | { kind: "InvalidLiteralCharacter"; segmentIndex: number; character: string }
  | { kind: "TooLong"; renderedMinLen: number }
  | { kind: "OnYearChangeWithoutYearSegment" }
  | { kind: "InvalidStartValue" };

/** NAV `invoiceNumber` XSD pattern: `[0-9A-Za-z\-/]{1,50}`. ASCII
 * letters + digits + dash + slash only. Backslash, dot, underscore,
 * space, etc. are rejected — the builder UI shows an inline error
 * when the operator types one in a Literal. */
const NAV_INVOICE_NUMBER_CHAR = /^[0-9A-Za-z\-/]$/;
const NAV_INVOICE_NUMBER_MAX_LEN = 50;

export function isNavInvoiceNumberChar(c: string): boolean {
  return NAV_INVOICE_NUMBER_CHAR.test(c);
}

/** Default template — matches the Rust `default_template()` byte-for-
 * byte. Renders to `INV-default/00001` at sequence 1, year irrelevant.
 * A tenant without a `[seller.numbering]` section in seller.toml gets
 * this from `GET /api/seller/numbering`. */
export function defaultTemplate(): NumberingTemplate {
  return {
    segments: [
      { kind: "Literal", text: "INV-default/" },
      { kind: "Counter", pad_width: 5 },
    ],
    reset_policy: "never",
    start_value: 1,
  };
}

/** Pure render. Matches the Rust `NumberingTemplate::render` shape
 * exactly so the SPA's live preview reflects what the backend would
 * emit on the wire. Pad is a FLOOR, not a cap — overflow grows
 * (`01`..`99`..`100`..) naturally via `padStart`. */
export function renderTemplate(
  template: NumberingTemplate,
  year: number,
  sequence: number,
): string {
  let out = "";
  for (const seg of template.segments) {
    switch (seg.kind) {
      case "Literal":
        out += seg.text;
        break;
      case "Year":
        if (seg.digits === 2) {
          const modded = ((year % 100) + 100) % 100;
          out += String(modded).padStart(2, "0");
        } else {
          // 4-digit form; pad with zeros for any year < 1000.
          out += String(year).padStart(4, "0");
        }
        break;
      case "Counter": {
        const width = Math.max(1, seg.pad_width);
        out += String(sequence).padStart(width, "0");
        break;
      }
      default: {
        // Closed-vocab exhaustiveness pin — adding a new kind without
        // widening this switch is a compile error.
        const _exhaustive: never = seg;
        throw new Error(`unhandled segment kind: ${JSON.stringify(_exhaustive)}`);
      }
    }
  }
  return out;
}

/** S165 — invoice-number build prefix. Mirrors the Rust
 * `build_profile::INVOICE_NUMBER_TEST_PREFIX`: dev/test builds prepend
 * `TEST-` (NAV-charset-legal hyphen, never underscore), production
 * builds prepend nothing. The SPA learns `isProductionBuild` from the
 * `GET /health` response. */
export function invoiceNumberBuildPrefix(isProductionBuild: boolean): string {
  return isProductionBuild ? "" : "TEST-";
}

/** S165 — render WITH the build prefix applied, mirroring the Rust
 * `NumberingTemplate::render_for_build`. The Tenant-Settings live
 * preview uses this so what the operator sees matches what the backend
 * actually emits onto the NAV wire: `TEST-ABERP/2026/0042` on a dev/test
 * build, `ABERP/2026/0042` on a production build. */
export function renderTemplateForBuild(
  template: NumberingTemplate,
  year: number,
  sequence: number,
  isProductionBuild: boolean,
): string {
  return (
    invoiceNumberBuildPrefix(isProductionBuild) +
    renderTemplate(template, year, sequence)
  );
}

/** Validate a template against the same ADR-0045 §3 invariants the
 * Rust validator enforces. Returns `null` when ok; the first found
 * `NumberingValidationError` otherwise. The SPA save button uses this
 * as a pre-PUT gate so the operator sees inline errors before the
 * roundtrip; the backend re-validates as the load-bearing gate. */
export function validateTemplate(
  template: NumberingTemplate,
): NumberingValidationError | null {
  if (template.segments.length === 0) {
    return { kind: "EmptyTemplate" };
  }
  if (template.start_value === 0 || template.start_value < 0) {
    return { kind: "InvalidStartValue" };
  }
  let counterCount = 0;
  let hasYear = false;
  for (let idx = 0; idx < template.segments.length; idx++) {
    const seg = template.segments[idx];
    if (seg.kind === "Counter") {
      counterCount += 1;
    } else if (seg.kind === "Year") {
      hasYear = true;
    } else if (seg.kind === "Literal") {
      if (seg.text.length === 0) {
        return { kind: "EmptyLiteral", segmentIndex: idx };
      }
      for (const c of seg.text) {
        if (!isNavInvoiceNumberChar(c)) {
          return {
            kind: "InvalidLiteralCharacter",
            segmentIndex: idx,
            character: c,
          };
        }
      }
    }
  }
  if (counterCount === 0) {
    return { kind: "NoCounter" };
  }
  if (counterCount > 1) {
    return { kind: "MultipleCounters", count: counterCount };
  }
  if (template.reset_policy === "on_year_change" && !hasYear) {
    return { kind: "OnYearChangeWithoutYearSegment" };
  }
  const minRender = renderTemplate(template, 9999, template.start_value);
  if (minRender.length > NAV_INVOICE_NUMBER_MAX_LEN) {
    return { kind: "TooLong", renderedMinLen: minRender.length };
  }
  return null;
}

/** Bilingual operator message for a validation error. Mirrors the
 * Rust `NumberingError::operator_message` shape so the SPA's inline
 * error pill reads identically to the backend's loud-fail message
 * when a PUT trips the server-side validator. */
export function errorMessage(err: NumberingValidationError): string {
  switch (err.kind) {
    case "EmptyTemplate":
      return "A sablon legalább egy szegmenst kell tartalmazzon.\nThe template must contain at least one segment.";
    case "NoCounter":
      return "A sablonnak pontosan egy számlálót (Counter) kell tartalmaznia. A sorszám nélkül a kiadott számlák száma ütközne.\nThe template must contain exactly one Counter segment. Without a counter, issued invoice numbers would collide.";
    case "MultipleCounters":
      return `A sablon ${err.count} db számlálót tartalmaz; pontosan egy szükséges.\nThe template contains ${err.count} Counter segments; exactly one is required.`;
    case "EmptyLiteral":
      return `A(z) ${err.segmentIndex + 1}. szöveg-szegmens üres.\nLiteral segment #${err.segmentIndex + 1} is empty.`;
    case "InvalidLiteralCharacter":
      return `A(z) ${err.segmentIndex + 1}. szöveg-szegmens érvénytelen karaktert tartalmaz: '${err.character}'. Engedélyezett: A-Z, a-z, 0-9, kötőjel (-), perjel (/).\nLiteral segment #${err.segmentIndex + 1} contains an invalid character: '${err.character}'. Allowed: A-Z, a-z, 0-9, dash (-), slash (/).`;
    case "TooLong":
      return `A sablon kimenete legalább ${err.renderedMinLen} karakter, ami meghaladja a NAV invoiceNumber 50-karakteres korlátját.\nThe template renders at least ${err.renderedMinLen} characters, exceeding the NAV invoiceNumber 50-character limit.`;
    case "OnYearChangeWithoutYearSegment":
      return "Az 'évváltáskor nullázódik' beállítás csak akkor használható, ha a sablon tartalmaz év (Year) szegmenst.\nThe 'reset on year change' policy requires the template to contain a Year segment.";
    case "InvalidStartValue":
      return "A kezdő érték nem lehet nulla; a számláló 1-től vagy nagyobb értéktől indul.\nStart value must be >= 1; the counter cannot begin at zero.";
    default: {
      const _exhaustive: never = err;
      return `unknown error: ${JSON.stringify(_exhaustive)}`;
    }
  }
}

/** Move a segment one position toward the start of the list.
 * No-op when at index 0. Returns a new array (pure). */
export function moveSegmentUp(
  segments: NumberingSegment[],
  index: number,
): NumberingSegment[] {
  if (index <= 0 || index >= segments.length) return segments;
  const next = segments.slice();
  const tmp = next[index - 1];
  next[index - 1] = next[index];
  next[index] = tmp;
  return next;
}

/** Move a segment one position toward the end of the list.
 * No-op when at last index. Returns a new array (pure). */
export function moveSegmentDown(
  segments: NumberingSegment[],
  index: number,
): NumberingSegment[] {
  if (index < 0 || index >= segments.length - 1) return segments;
  const next = segments.slice();
  const tmp = next[index + 1];
  next[index + 1] = next[index];
  next[index] = tmp;
  return next;
}

/** Remove a segment by index. Returns a new array (pure). */
export function removeSegment(
  segments: NumberingSegment[],
  index: number,
): NumberingSegment[] {
  if (index < 0 || index >= segments.length) return segments;
  return [...segments.slice(0, index), ...segments.slice(index + 1)];
}
