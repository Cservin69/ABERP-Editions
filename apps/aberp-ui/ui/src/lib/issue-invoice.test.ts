// PR-44ζ / session-59 — vitest pin for the IssueInvoice form-to-
// request-body composer.
//
// The composer is the load-bearing seam between the SPA form state
// and the wire `IssueInvoiceRequest` shape. A regression that
// renames a field (or drops the trim, or mis-maps the currency)
// would surface as a 400 from the backend with a confusing error
// rather than a silent bad-issuance — but the test catches it at
// gate time before any backend roundtrip.
//
// Mirror invariant per A156 / A161: the backend's
// `serve::IssueInvoiceRequest` Deserialize and this composer agree
// on the wire field names (camelCase JSON, UPPERCASE currency).

import { describe, expect, it } from "vitest";

import {
  cannotIssueDueToBank,
  composeIssueInvoiceBody,
  emptyForm,
  parseInvoicePreflightErrors,
  parseMissingSellerConfigError,
  targetForFieldPath,
  type InvoicePreflightErrorKind,
} from "./issue-invoice";

describe("composeIssueInvoiceBody", () => {
  it("reshapes HUF form state into the wire body verbatim", () => {
    // PR-53 / session-73 — supplier removed from the form + wire
    // shape; backend reads seller identity from seller.toml server-
    // side. The composer's output is customer + lines + currency.
    const form = {
      ...emptyForm(),
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      currency: "HUF" as const,
      lines: [
        {
          description: "Widget A",
          quantity: 2,
          unitPriceMinor: 1000,
          vatRatePercent: 27,
          note: "",
        },
      ],
    };

    const body = composeIssueInvoiceBody(form);

    expect(body).toEqual({
      customer: {
        taxNumber: "87654321-2-13",
        name: "Vevő Kft.",
        // PR-77 — `address: undefined` flows out of `composeCustomerAddress`
        // when the form's address fields are all blank.
        address: undefined,
      },
      lines: [
        {
          description: "Widget A",
          quantity: 2,
          unitPrice: 1000,
          vatRatePercent: 27,
          // PR-82 — blank line note normalises to `null` on the wire
          // so the backend sees a clean "no note" signal.
          note: null,
        },
      ],
      currency: "HUF",
      bankAccountId: null,
      // PR-82 — blank invoice-level note normalises to `null`.
      invoiceNote: null,
    });
  });

  // PR-73 / ADR-0040 §addendum — bank-picker composer pins.
  it("emits bankAccountId verbatim when the picker has a selection", () => {
    const form = {
      ...emptyForm(),
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      bankAccountId: "bnk_01ARZ3NDEKTSV4RRFFQ69G5FAV",
      invoiceNote: "",
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.bankAccountId).toBe("bnk_01ARZ3NDEKTSV4RRFFQ69G5FAV");
  });

  it("normalises empty-string bankAccountId to null on the wire", () => {
    // The picker writes `null` for "no selection"; an empty-string
    // residue (e.g., from a previous-row edit) must NOT reach the
    // backend as `bankAccountId: ""` — the backend resolver treats
    // empty-string as missing-field and falls back to the per-currency
    // default, but emitting `null` explicitly keeps the wire clean.
    const form = {
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      bankAccountId: "   ",
      invoiceNote: "",
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.bankAccountId).toBeNull();
  });

  it("composes null bankAccountId when picker has no selection", () => {
    const body = composeIssueInvoiceBody({
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
    });
    expect(body.bankAccountId).toBeNull();
  });

  // PR-82 — buyer-facing notes composer pins.
  it("emits per-line and per-invoice notes verbatim when the operator typed text", () => {
    const form = {
      ...emptyForm(),
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      invoiceNote: "Köszönjük a vásárlást!",
      lines: [
        {
          description: "Widget A",
          quantity: 1,
          unitPriceMinor: 1000,
          vatRatePercent: 27,
          note: "Please ship to dock B",
        },
      ],
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.invoiceNote).toBe("Köszönjük a vásárlást!");
    expect(body.lines[0].note).toBe("Please ship to dock B");
  });

  it("trims whitespace and normalises blank notes to null on the wire", () => {
    // Whitespace-only notes are the operator's "I almost typed
    // something then deleted it" residue — the wire should not
    // carry empty strings.
    const form = {
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      invoiceNote: "   \n  ",
      lines: [
        {
          description: "Widget A",
          quantity: 1,
          unitPriceMinor: 100,
          vatRatePercent: 27,
          note: "  ",
        },
      ],
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.invoiceNote).toBeNull();
    expect(body.lines[0].note).toBeNull();
  });

  it("trims surrounding whitespace on notes while preserving inner content", () => {
    const form = {
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      invoiceNote: "  Pay by NET 30.  ",
      lines: [
        {
          description: "Widget A",
          quantity: 1,
          unitPriceMinor: 100,
          vatRatePercent: 27,
          note: "  Line A note  ",
        },
      ],
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.invoiceNote).toBe("Pay by NET 30.");
    expect(body.lines[0].note).toBe("Line A note");
  });

  it("does not emit a supplier field on the wire body (PR-53)", () => {
    // Regression guard for the PR-53 cross-cutting fix: the SPA must
    // NOT send supplier in the wire body. A drift here would
    // re-introduce the "obsolete to retype seller after wizard"
    // operator-feedback that PR-53 closed.
    const body = composeIssueInvoiceBody({
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
    });
    expect("supplier" in body).toBe(false);
  });

  it("emits EUR currency verbatim on the EUR branch", () => {
    const form = {
      ...emptyForm(),
      customerName: "EU Buyer GmbH",
      customerTaxNumber: "DE123456789",
      currency: "EUR" as const,
      lines: [
        {
          description: "Consulting (1h)",
          quantity: 8,
          unitPriceMinor: 12500, // 125.00 EUR in cents
          vatRatePercent: 27,
          note: "",
        },
      ],
    };

    const body = composeIssueInvoiceBody(form);

    expect(body.currency).toBe("EUR");
    expect(body.lines[0]).toEqual({
      description: "Consulting (1h)",
      quantity: 8,
      unitPrice: 12500,
      vatRatePercent: 27,
      // PR-82 — blank-after-trim ⇒ null on the wire.
      note: null,
    });
  });

  it("trims whitespace on every string field the backend validates", () => {
    // Backend `validate_issue_request` `.trim()`-checks customer
    // name + tax number; the composer pre-trims so the wire body
    // matches what the backend reads. A regression that drops a
    // trim would let a `"  "` value pass the SPA's required-field
    // check but fail the backend's.
    const form = {
      ...emptyForm(),
      customerName: "  Trimmed Customer  ",
      customerTaxNumber: "  87654321-2-13  ",
      currency: "HUF" as const,
      lines: [
        {
          description: "  Trimmed description  ",
          quantity: 1,
          unitPriceMinor: 500,
          vatRatePercent: 27,
          note: "",
        },
      ],
    };

    const body = composeIssueInvoiceBody(form);

    expect(body.customer.name).toBe("Trimmed Customer");
    expect(body.customer.taxNumber).toBe("87654321-2-13");
    expect(body.lines[0].description).toBe("Trimmed description");
  });

  // PR-77 / session-101 — customerAddress quartet on the wire body.
  /** When all four address fields are populated, the wire body carries
   * the camelCase address shape. NAV's `CUSTOMER_DATA_EXPECTED`
   * business rule (the rule that ABORTED invoice 18) requires this
   * block for any Hungarian-business buyer; the composer's job is to
   * pass it through verbatim so the backend's preflight + emitter can
   * see the same shape the operator authored. */
  it("emits customer.address verbatim when populated", () => {
    const form = {
      ...emptyForm(),
      customerName: "AZ9 Services",
      customerTaxNumber: "27952890-2-42",
      customerCountryCode: "HU",
      customerPostalCode: "1097",
      customerCity: "Budapest",
      customerStreet: "Üllői út 1.",
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.customer.address).toEqual({
      countryCode: "HU",
      postalCode: "1097",
      city: "Budapest",
      street: "Üllői út 1.",
    });
  });

  /** PR-77 / session-101 — empty quartet → field omitted entirely.
   * The backend preflight surfaces `CustomerAddressMissing` on the
   * absent field rather than on a body with four empty strings (the
   * cleaner operator message; consistent with the SPA's per-field
   * error renderer). */
  it("omits customer.address when every sub-field is blank", () => {
    const form = {
      ...emptyForm(),
      customerName: "AZ9",
      customerTaxNumber: "27952890-2-42",
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.customer.address).toBeUndefined();
  });

  /** PR-77 / session-101 — partially-populated address: the SPA still
   * sends what's there (the backend's per-field preflight names the
   * exact gap). A future scope tightening could promote the SPA's own
   * required-attribute check to mirror the backend; for now the SPA
   * trusts the backend's preflight to do the per-field naming. */
  it("emits customer.address with blank sub-fields trimmed when partially populated", () => {
    const form = {
      ...emptyForm(),
      customerName: "AZ9",
      customerTaxNumber: "27952890-2-42",
      customerCountryCode: "HU",
      customerPostalCode: "  ",
      customerCity: "Budapest",
      customerStreet: "",
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.customer.address).toEqual({
      countryCode: "HU",
      postalCode: "",
      city: "Budapest",
      street: "",
    });
  });

  it("preserves all lines when there are multiple", () => {
    // Per-line ordering matters — the backend stamps the lines in
    // the order received. A regression that re-ordered or
    // deduplicated would corrupt the invoice silently.
    const form = {
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      currency: "HUF" as const,
      lines: [
        { description: "A", quantity: 1, unitPriceMinor: 100, vatRatePercent: 27, note: "" },
        { description: "B", quantity: 2, unitPriceMinor: 200, vatRatePercent: 5, note: "" },
        { description: "C", quantity: 3, unitPriceMinor: 300, vatRatePercent: 0, note: "" },
      ],
    };

    const body = composeIssueInvoiceBody(form);

    expect(body.lines.length).toBe(3);
    expect(body.lines.map((l) => l.description)).toEqual(["A", "B", "C"]);
    expect(body.lines.map((l) => l.unitPrice)).toEqual([100, 200, 300]);
    expect(body.lines.map((l) => l.vatRatePercent)).toEqual([27, 5, 0]);
  });
});

// PR-50 / session-70 — pins for the typed `missing_seller_config`
// error parser. The SPA's IssueInvoice modal calls
// `parseMissingSellerConfigError` on every catch arm; a regression
// that mis-detects the discriminant (or fails to extract the
// config_path + sample_path hints) would silently degrade to the
// raw-string error display, defeating the whole point of the typed
// 400 body. Per CLAUDE.md rule 9 — happy path + each rejection arm
// pinned.
describe("parseMissingSellerConfigError", () => {
  it("extracts the typed body from the Tauri-wrapped error string", () => {
    // The Tauri forward helper wraps the backend's JSON body in a
    // human-readable prefix; the parser strips the prefix and parses
    // the embedded JSON object.
    const raw =
      'backend returned 400 Bad Request for /invoices/issue: ' +
      '{"error":"missing_seller_config",' +
      '"message":"supplier tax number `24904362` is not a valid ' +
      'Hungarian ADÓSZÁM (expected three dash-separated segments; ' +
      "expected `xxxxxxxx-y-zz`, e.g. `24904362-2-41`)\"," +
      '"config_path":"/Users/aben/.aberp/test/seller.toml",' +
      '"sample_path":"/Users/aben/Documents/Claude/Projects/ABERP/' +
      'samples/seller.toml.example"}';
    const parsed = parseMissingSellerConfigError(raw);
    expect(parsed).not.toBeNull();
    expect(parsed!.error).toBe("missing_seller_config");
    expect(parsed!.config_path).toBe(
      "/Users/aben/.aberp/test/seller.toml",
    );
    expect(parsed!.sample_path).toBe(
      "/Users/aben/Documents/Claude/Projects/ABERP/samples/seller.toml.example",
    );
    expect(parsed!.message).toContain("24904362");
    expect(parsed!.message).toContain("xxxxxxxx-y-zz");
  });

  it("returns null for a plain `{error: ...}` 400 body", () => {
    // Pre-PR-50 400 bodies (empty lines, missing customer name) carry
    // only the `error` discriminant string. The parser must NOT
    // misidentify those as the typed shape — the SPA falls back to
    // the raw-string display.
    const raw =
      'backend returned 400 Bad Request for /invoices/issue: ' +
      '{"error":"at least one line item is required"}';
    expect(parseMissingSellerConfigError(raw)).toBeNull();
  });

  it("returns null when the error body is malformed JSON", () => {
    const raw = "backend returned 500 Internal Server Error: <html>...</html>";
    expect(parseMissingSellerConfigError(raw)).toBeNull();
  });

  it("returns null when the discriminant is present but hints are missing", () => {
    // A backend drift that emits the discriminant without the
    // hint fields would surface here — fall back to raw-string
    // display rather than rendering a broken `undefined` path
    // (CLAUDE.md rule 12, fail loud).
    const raw =
      'backend returned 400 Bad Request for /invoices/issue: ' +
      '{"error":"missing_seller_config","message":"X"}';
    expect(parseMissingSellerConfigError(raw)).toBeNull();
  });
});

// PR-69 / session-91 — pins for the typed `invoice_preflight_failed`
// error parser + the field-path → form-target router (ADR-0038). The
// SPA's IssueInvoice form calls `parseInvoicePreflightErrors` on every
// catch arm; a regression that mis-detects the discriminant or that
// silently coerces an unknown `kind` to a known one would degrade the
// inline-error rendering to the raw-string fallback. Per CLAUDE.md
// rule 9 — per-variant + per-rejection-arm assertions.

function preflightBodyJson(items: Array<Record<string, string>>): string {
  return (
    'backend returned 400 Bad Request for /invoices/issue: ' +
    JSON.stringify({
      error: "invoice_preflight_failed",
      errors: items,
    })
  );
}

describe("parseInvoicePreflightErrors — per-variant rendering pins", () => {
  // One pin per InvoicePreflightErrorKind. Pinning the round-trip from
  // the wire shape into the typed body proves the closed-vocab kind
  // guard recognizes every variant; a regression that drops a variant
  // from `isKnownPreflightKind` would surface as `null` here.
  const variants: Array<{
    kind: InvoicePreflightErrorKind;
    field_path: string;
    message_hu: string;
    message_en: string;
  }> = [
    {
      kind: "CustomerNameEmpty",
      field_path: "customer.name",
      message_hu: "Az ügyfél neve kötelező.",
      message_en: "Customer name is required.",
    },
    {
      kind: "CustomerTaxNumberMissing",
      field_path: "customer.taxNumber",
      message_hu:
        "Az ügyfél adószáma (ADÓSZÁM) kötelező (helyes: `xxxxxxxx-y-zz`, pl. `87654321-2-13`).",
      message_en:
        "Customer ADÓSZÁM is required (expected `xxxxxxxx-y-zz`, e.g. `87654321-2-13`).",
    },
    {
      kind: "CustomerTaxNumberMalformed",
      field_path: "customer.taxNumber",
      message_hu:
        "Az ügyfél adószáma (`1234`) hibás formátum (három, kötőjellel elválasztott szegmens szükséges). Helyes: `xxxxxxxx-y-zz`, pl. `87654321-2-13`.",
      message_en:
        "Customer ADÓSZÁM `1234` is not a valid Hungarian tax number (expected three dash-separated segments); expected `xxxxxxxx-y-zz`, e.g. `87654321-2-13`.",
    },
    {
      kind: "InvoiceLinesEmpty",
      field_path: "lines",
      message_hu: "Legalább egy tételsor szükséges a számlához.",
      message_en: "At least one line item is required.",
    },
    {
      kind: "LineItemDescriptionEmpty",
      field_path: "lines[0].description",
      message_hu: "A(z) 1. tételsor megnevezése kötelező.",
      message_en: "Line 1 description is required.",
    },
    {
      kind: "LineItemQuantityZero",
      field_path: "lines[0].quantity",
      message_hu: "A(z) 1. tételsor mennyisége legalább 1 kell legyen.",
      message_en: "Line 1 quantity must be at least 1.",
    },
    {
      kind: "LineItemUnitPriceNonPositive",
      field_path: "lines[0].unitPrice",
      message_hu:
        "A(z) 1. tételsor egységára pozitív kell legyen (kapott: 0). Sztornó / módosítás külön folyamat.",
      message_en:
        "Line 1 unit price must be positive (got 0). Storno / modification is a separate flow.",
    },
    {
      kind: "LineItemVatRateUnknown",
      field_path: "lines[0].vatRatePercent",
      message_hu:
        "A(z) 1. tételsor ÁFA-kulcsa (12%) nem szerepel a magyar szabványos kulcsok között (0%, 5%, 18%, 27%). Speciális kategóriák (AAM/TAM/TAH) jelenleg nem támogatottak.",
      message_en:
        "Line 1 VAT rate (12%) is not a Hungarian standard rate (allowed: 0%, 5%, 18%, 27%). Special categories (AAM/TAM/TAH) are not supported on this wire shape today.",
    },
    // PR-73 / ADR-0040 §addendum — bank-related variants.
    {
      kind: "SellerBankMissingForCurrency",
      field_path: "bankAccountId",
      message_hu: "Nincs konfigurált bankszámla a számla pénzneméhez (EUR).",
      message_en: "No bank account configured for the invoice's currency (EUR).",
    },
    {
      kind: "SellerBankCurrencyMismatch",
      field_path: "bankAccountId",
      message_hu:
        "A választott bankszámla (`bnk_xyz`) pénzneme HUF eltér a számla pénznemétől EUR.",
      message_en:
        "Selected bank account (`bnk_xyz`) currency HUF does not match the invoice currency EUR.",
    },
  ];

  for (const v of variants) {
    it(`parses ${v.kind} into the typed body with HU + EN messages`, () => {
      const parsed = parseInvoicePreflightErrors(preflightBodyJson([v]));
      expect(parsed).not.toBeNull();
      expect(parsed!.error).toBe("invoice_preflight_failed");
      expect(parsed!.errors.length).toBe(1);
      expect(parsed!.errors[0].kind).toBe(v.kind);
      expect(parsed!.errors[0].field_path).toBe(v.field_path);
      expect(parsed!.errors[0].message_hu).toBe(v.message_hu);
      expect(parsed!.errors[0].message_en).toBe(v.message_en);
    });
  }

  it("collects multiple errors in array order (no dedup, no reorder)", () => {
    const raw = preflightBodyJson([
      {
        kind: "CustomerNameEmpty",
        field_path: "customer.name",
        message_hu: "x",
        message_en: "y",
      },
      {
        kind: "LineItemQuantityZero",
        field_path: "lines[0].quantity",
        message_hu: "x",
        message_en: "y",
      },
      {
        kind: "LineItemVatRateUnknown",
        field_path: "lines[2].vatRatePercent",
        message_hu: "x",
        message_en: "y",
      },
    ]);
    const parsed = parseInvoicePreflightErrors(raw);
    expect(parsed!.errors.length).toBe(3);
    expect(parsed!.errors.map((e) => e.kind)).toEqual([
      "CustomerNameEmpty",
      "LineItemQuantityZero",
      "LineItemVatRateUnknown",
    ]);
  });
});

describe("parseInvoicePreflightErrors — rejection arms", () => {
  it("returns null for the PR-50 missing_seller_config 400 body", () => {
    // The two typed 400 shapes coexist on the same route; the
    // preflight parser must not misidentify a seller-config 400 as
    // its own shape. The caller falls through to
    // `parseMissingSellerConfigError`.
    const raw =
      'backend returned 400 Bad Request for /invoices/issue: ' +
      '{"error":"missing_seller_config","message":"x","config_path":"a","sample_path":"b"}';
    expect(parseInvoicePreflightErrors(raw)).toBeNull();
  });

  it("returns null for a plain `{error: ...}` 400 body", () => {
    // Pre-PR-69 legacy 400 surface from `validate_issue_request`.
    const raw =
      'backend returned 400 Bad Request for /invoices/issue: ' +
      '{"error":"at least one line item is required"}';
    expect(parseInvoicePreflightErrors(raw)).toBeNull();
  });

  it("returns null when the body is malformed JSON", () => {
    expect(parseInvoicePreflightErrors("backend returned 500: <html>...")).toBeNull();
  });

  it("returns null when an error item carries an unknown `kind`", () => {
    // Backend drift that adds a variant without the SPA knowing about
    // it would surface here — fail loud rather than render `(unknown)`.
    const raw =
      'backend returned 400 Bad Request for /invoices/issue: ' +
      '{"error":"invoice_preflight_failed","errors":[' +
      '{"kind":"CustomerNameEmpty","field_path":"customer.name","message_hu":"x","message_en":"y"},' +
      '{"kind":"FutureUnknownVariant","field_path":"customer.name","message_hu":"x","message_en":"y"}' +
      ']}';
    expect(parseInvoicePreflightErrors(raw)).toBeNull();
  });

  it("returns null when an error item is missing a required field", () => {
    const raw =
      'backend returned 400 Bad Request for /invoices/issue: ' +
      '{"error":"invoice_preflight_failed","errors":[' +
      '{"kind":"CustomerNameEmpty","field_path":"customer.name","message_en":"y"}' +
      ']}';
    expect(parseInvoicePreflightErrors(raw)).toBeNull();
  });
});

describe("targetForFieldPath — closed-vocab router", () => {
  it("routes customer.name to the customer-name input", () => {
    expect(targetForFieldPath("customer.name")).toEqual({
      kind: "customer",
      field: "name",
    });
  });

  it("routes customer.taxNumber to the customer-tax input", () => {
    expect(targetForFieldPath("customer.taxNumber")).toEqual({
      kind: "customer",
      field: "taxNumber",
    });
  });

  it("routes the bare `lines` path to the line-list container", () => {
    expect(targetForFieldPath("lines")).toEqual({ kind: "lines" });
  });

  it("routes per-line paths to (lineIndex, field) tuples", () => {
    expect(targetForFieldPath("lines[0].description")).toEqual({
      kind: "line",
      lineIndex: 0,
      field: "description",
    });
    expect(targetForFieldPath("lines[3].vatRatePercent")).toEqual({
      kind: "line",
      lineIndex: 3,
      field: "vatRatePercent",
    });
    expect(targetForFieldPath("lines[12].unitPrice")).toEqual({
      kind: "line",
      lineIndex: 12,
      field: "unitPrice",
    });
    expect(targetForFieldPath("lines[7].quantity")).toEqual({
      kind: "line",
      lineIndex: 7,
      field: "quantity",
    });
  });

  it("returns null for paths outside the closed-vocab (forward-compat fallback)", () => {
    // A future preflight variant whose field_path the SPA doesn't yet
    // route maps to null; the renderer surfaces it in the general
    // error block rather than dropping it.
    expect(targetForFieldPath("customer.address.city")).toBeNull();
    expect(targetForFieldPath("lines[0].newFutureField")).toBeNull();
    expect(targetForFieldPath("issueDate")).toBeNull();
    expect(targetForFieldPath("")).toBeNull();
    expect(targetForFieldPath("lines[abc].description")).toBeNull();
  });

  // PR-73 / ADR-0040 §addendum — bank-picker field-path routing.
  it("routes bankAccountId to the bank-picker target", () => {
    expect(targetForFieldPath("bankAccountId")).toEqual({
      kind: "bankAccountId",
    });
  });
});

// PR-73 / ADR-0040 §addendum — bank-related preflight kinds must round-
// trip through the typed body parser. A regression that drops one of
// the two new kinds from `isKnownPreflightKind` would surface here.
describe("parseInvoicePreflightErrors — PR-73 bank-related variants", () => {
  it("parses SellerBankMissingForCurrency with bilingual messages", () => {
    const raw = preflightBodyJson([
      {
        kind: "SellerBankMissingForCurrency",
        field_path: "bankAccountId",
        message_hu:
          "Nincs konfigurált bankszámla a számla pénzneméhez (EUR). " +
          "Adjon meg egy `[[seller.banks]]` bejegyzést ehhez a pénznemhez a " +
          "Bérlőbeállítások / Bank accounts menüpontban.",
        message_en:
          "No bank account configured for the invoice's currency (EUR). " +
          "Add a `[[seller.banks]]` entry for this currency in Tenant " +
          "Settings → Bank accounts.",
      },
    ]);
    const parsed = parseInvoicePreflightErrors(raw);
    expect(parsed).not.toBeNull();
    expect(parsed!.errors[0].kind).toBe("SellerBankMissingForCurrency");
    expect(parsed!.errors[0].field_path).toBe("bankAccountId");
    expect(parsed!.errors[0].message_hu).toContain("EUR");
    expect(parsed!.errors[0].message_en).toContain("EUR");
    expect(parsed!.errors[0].message_en).toContain("Tenant Settings");
  });

  it("parses SellerBankCurrencyMismatch with selected_id + both currencies", () => {
    const raw = preflightBodyJson([
      {
        kind: "SellerBankCurrencyMismatch",
        field_path: "bankAccountId",
        message_hu:
          "A választott bankszámla (`bnk_xyz`) pénzneme HUF eltér a számla " +
          "pénznemétől EUR. Válasszon olyan bankszámlát, amelynek pénzneme " +
          "megegyezik a számla pénznemével.",
        message_en:
          "Selected bank account (`bnk_xyz`) currency HUF does not match " +
          "the invoice currency EUR. Pick a bank account whose currency " +
          "matches the invoice.",
      },
    ]);
    const parsed = parseInvoicePreflightErrors(raw);
    expect(parsed).not.toBeNull();
    expect(parsed!.errors[0].kind).toBe("SellerBankCurrencyMismatch");
    expect(parsed!.errors[0].field_path).toBe("bankAccountId");
    expect(parsed!.errors[0].message_hu).toContain("bnk_xyz");
    expect(parsed!.errors[0].message_en).toContain("HUF");
    expect(parsed!.errors[0].message_en).toContain("EUR");
  });
});

// PR-75 / session-99 — Submit-button gate for the bank-picker branch.
// Pins the regression Ervin caught: clicking "Issue invoice" when no
// bank entry exists for the form's currency silently fired a request
// with no inline feedback. The IssueInvoice.svelte template threads the
// derived value of `cannotIssueDueToBank` onto `<button disabled>` so
// the button is unclickable; these tests pin the decision.
describe("cannotIssueDueToBank — Submit gate when bank picker is unresolvable", () => {
  it("blocks Submit while banks are still loading (first dialog open)", () => {
    // sellerBanksLoaded=false is the in-flight state. The picker
    // renders 'Loading bank accounts…'; the operator clicking Submit
    // before the fetch resolves must not race past the bank check.
    const blocked = cannotIssueDueToBank({
      sellerBanksLoaded: false,
      sellerBanksLoadError: null,
      banksForCurrencyCount: 0,
    });
    expect(blocked).toBe(true);
  });

  it("blocks Submit when banks load failed (sellerBanksLoadError set)", () => {
    // PR-74 added a Retry affordance for this branch; PR-75 closes the
    // companion footgun where Submit was still clickable. Disabling
    // Submit forces the operator to Retry or close the dialog first.
    const blocked = cannotIssueDueToBank({
      sellerBanksLoaded: true,
      sellerBanksLoadError: "Network error",
      banksForCurrencyCount: 0,
    });
    expect(blocked).toBe(true);
  });

  it("blocks Submit when banks loaded but zero entries exist for currency", () => {
    // Live regression: HUF banks configured, operator switches the
    // form's currency to EUR, picker renders the "no bank for currency"
    // hint with the Tenant-Settings link. Pre-PR-75 Submit was still
    // clickable + fired silently. Post-PR-75 it's disabled.
    const blocked = cannotIssueDueToBank({
      sellerBanksLoaded: true,
      sellerBanksLoadError: null,
      banksForCurrencyCount: 0,
    });
    expect(blocked).toBe(true);
  });

  it("allows Submit once banks loaded AND at least one entry exists for currency", () => {
    // The happy path: bank picker populated, operator can issue.
    const blocked = cannotIssueDueToBank({
      sellerBanksLoaded: true,
      sellerBanksLoadError: null,
      banksForCurrencyCount: 1,
    });
    expect(blocked).toBe(false);
  });

  it("allows Submit with multiple entries for currency (operator chose a non-default)", () => {
    // Multi-bank-per-currency case (e.g., two HUF accounts): the
    // picker shows a dropdown; the operator's selection sets
    // form.bankAccountId. The gate only cares about presence-for-
    // currency, not the operator's specific pick.
    const blocked = cannotIssueDueToBank({
      sellerBanksLoaded: true,
      sellerBanksLoadError: null,
      banksForCurrencyCount: 3,
    });
    expect(blocked).toBe(false);
  });
});
