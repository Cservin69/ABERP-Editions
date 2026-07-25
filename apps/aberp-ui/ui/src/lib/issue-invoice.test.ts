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
  applyProductPick,
  applyVatKind,
  cannotIssueDueToBank,
  composeIssueInvoiceBody,
  deliveryDateOverrideFor,
  emptyForm,
  lineCurrencyMismatchWarning,
  parseInvoicePreflightErrors,
  parseMissingSellerConfigError,
  paymentDeadlineFromOffset,
  resolveBankForCurrency,
  targetForFieldPath,
  vatKindBuyerMismatch,
  VAT_KIND_OPTIONS,
  vatKindLocksRate,
  type InvoicePreflightErrorKind,
  type IssueInvoiceFormState,
} from "./issue-invoice";
import type { Product, SellerBankResponse, VatRateKindBody } from "./api";

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
          quantityInput: "2",
          // PR-88 / session-113 — operator-typed raw string. HUF is
          // 0-decimal, so `"1000"` parses to 1000 minor units (= 1000
          // forints) — same wire output as the pre-PR-88
          // `unitPriceMinor: 1000` posture.
          unitPriceInput: "1000",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          note: "",
        },
      ],
    };

    const body = composeIssueInvoiceBody(form);

    expect(body).toEqual({
      customer: {
        // PR-97 / ADR-0048 — closed-vocab buyer-kind. `emptyForm()`
        // seeds `Domestic` so the legacy golden body assertion gains
        // the field.
        vatStatus: "Domestic",
        // PR-97 / ADR-0048 (Ervin override 1) — `null` because the
        // test form spread `emptyForm()` without invoking
        // `pickPartner` (one-off buyer, no saved-partner id).
        partnerId: null,
        taxNumber: "87654321-2-13",
        name: "Vevő Kft.",
        // PR-77 — `address: undefined` flows out of `composeCustomerAddress`
        // when the form's address fields are all blank.
        address: undefined,
      },
      lines: [
        {
          description: "Widget A",
          quantity: "2",
          unitPrice: 1000,
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          // PR-82 — blank line note normalises to `null` on the wire
          // so the backend sees a clean "no note" signal.
          note: null,
          // S159 — no product picked on this one-off line, so `unit`
          // is `null`; the backend's NAV emit falls back to PIECE.
          unit: null,
        },
      ],
      currency: "HUF",
      bankAccountId: null,
      // PR-82 — blank invoice-level note normalises to `null`.
      invoiceNote: null,
      // PR-84 — the form's seeded dates are emitted verbatim. The
      // composer does NOT carry the form's `invoiceDate` (server
      // stamps it); only payment_deadline + delivery_date + the
      // comfort-zone override discriminant. The seeded defaults from
      // `emptyForm()` flow through: paymentDeadline = today+8;
      // deliveryDate = today; override = null (in range).
      paymentDeadline: form.paymentDeadline,
      deliveryDate: form.deliveryDate,
      deliveryDateOverride: null,
      // PR-92 / ADR-0047 — default-on auto-send toggle. The composer
      // emits `true` because `emptyForm` seeds the flag to `true`
      // (silence-by-omission is the wrong default for a buyer-comms
      // product). The operator must explicitly un-check to suppress.
      emailBuyerOnIssue: true,
      // PR-99 Item 4 Part B — default-on submit-to-NAV toggle, same
      // mirror posture as `emailBuyerOnIssue`.
      submitToNavOnIssue: true,
      // S160 / ADR-0050 — payment method seeds to `TRANSFER` (Átutalás)
      // in `emptyForm`, matching the pre-S160 hardcoded emit.
      paymentMethod: "TRANSFER",
      // PR-203 / S203 — empty form has no recipient override; composer
      // emits `null` so the backend resolver falls back to partner.email.
      emailRecipientOverride: null,
    });
  });

  // S160 / ADR-0050 — the operator's payment-method dropdown choice
  // rides the wire body verbatim as the bare NAV token. Pins the
  // form→wire seam for the Fizetési mód selector.
  it("carries the operator-selected payment method (Készpénz/CASH)", () => {
    const form = {
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      paymentMethod: "CASH" as const,
    };
    expect(composeIssueInvoiceBody(form).paymentMethod).toBe("CASH");
    // Default (unchanged form) carries TRANSFER.
    expect(composeIssueInvoiceBody(emptyForm()).paymentMethod).toBe("TRANSFER");
  });

  // S157 — decimal quantity round-trips through the composer. Both `1.5`
  // and `1,5` typed must reach the wire as the canonical string `"1.5"`.
  it("composes a decimal quantity (1.5) to the canonical dot-decimal wire string", () => {
    const dotForm = {
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      lines: [
        {
          description: "Consulting",
          quantityInput: "1.5",
          unitPriceInput: "1000",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          note: "",
        },
      ],
    };
    const commaForm = {
      ...dotForm,
      lines: [{ ...dotForm.lines[0], quantityInput: "1,5" }],
    };
    expect(composeIssueInvoiceBody(dotForm).lines[0].quantity).toBe("1.5");
    // The headline requirement: `1,5` and `1.5` produce the same wire value.
    expect(composeIssueInvoiceBody(commaForm).lines[0].quantity).toBe("1.5");
  });

  it("composes an unparseable/zero quantity to \"0\" so the backend preflight fires", () => {
    const form = {
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      lines: [
        {
          description: "Bad qty",
          quantityInput: "",
          unitPriceInput: "1000",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          note: "",
        },
      ],
    };
    expect(composeIssueInvoiceBody(form).lines[0].quantity).toBe("0");
  });

  // PR-99 Item 4 Part B — submit-to-NAV toggle composer pins.
  it("emits submitToNavOnIssue=true from a default form (silence-by-omission cannot suppress)", () => {
    const body = composeIssueInvoiceBody({
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      customerCountryCode: "HU",
      customerPostalCode: "1052",
      customerCity: "Budapest",
      customerStreet: "Test 1.",
    });
    expect(body.submitToNavOnIssue).toBe(true);
  });

  it("emits submitToNavOnIssue=false when the operator unchecks the toggle", () => {
    const body = composeIssueInvoiceBody({
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      customerCountryCode: "HU",
      customerPostalCode: "1052",
      customerCity: "Budapest",
      customerStreet: "Test 1.",
      submitToNavOnIssue: false,
    });
    expect(body.submitToNavOnIssue).toBe(false);
  });

  // PR-92 / ADR-0047 — auto-send toggle composer pins.
  it("emits emailBuyerOnIssue=true from a default form (silence-by-omission cannot suppress)", () => {
    const body = composeIssueInvoiceBody({
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
    });
    expect(body.emailBuyerOnIssue).toBe(true);
  });

  it("emits emailBuyerOnIssue=false when the operator opted this invoice out", () => {
    const body = composeIssueInvoiceBody({
      ...emptyForm(),
      customerName: "C",
      customerTaxNumber: "y",
      emailBuyerOnIssue: false,
    });
    expect(body.emailBuyerOnIssue).toBe(false);
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
          quantityInput: "1",
          unitPriceInput: "1000",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
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
          quantityInput: "1",
          unitPriceInput: "100",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
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
          quantityInput: "1",
          unitPriceInput: "100",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
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
          quantityInput: "8",
          // PR-88 / session-113 — operator-typed raw string. EUR is
          // 2-decimal, so `"125.00"` parses to 12500 minor units
          // (= 125.00 EUR). The pre-PR-88 form stored 12500 in
          // `unitPriceMinor` directly; the new posture is "type
          // what's on the invoice, the parser converts."
          unitPriceInput: "125.00",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          note: "",
        },
      ],
    };

    const body = composeIssueInvoiceBody(form);

    expect(body.currency).toBe("EUR");
    expect(body.lines[0]).toEqual({
      description: "Consulting (1h)",
      quantity: "8",
      unitPrice: 12500,
      vatRatePercent: 27,
      vatRateKind: "Percent" as const,
      // PR-82 — blank-after-trim ⇒ null on the wire.
      note: null,
      // S159 — no product picked ⇒ unit null ⇒ PIECE fallback.
      unit: null,
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
          quantityInput: "1",
          unitPriceInput: "500",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
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
        { description: "A", quantityInput: "1", unitPriceInput: "100", vatRatePercent: 27, vatRateKind: "Percent" as const, note: "" },
        { description: "B", quantityInput: "2", unitPriceInput: "200", vatRatePercent: 5, vatRateKind: "Percent" as const, note: "" },
        { description: "C", quantityInput: "3", unitPriceInput: "300", vatRatePercent: 0, vatRateKind: "Percent" as const, note: "" },
      ],
    };

    const body = composeIssueInvoiceBody(form);

    expect(body.lines.length).toBe(3);
    expect(body.lines.map((l) => l.description)).toEqual(["A", "B", "C"]);
    expect(body.lines.map((l) => l.unitPrice)).toEqual([100, 200, 300]);
    expect(body.lines.map((l) => l.vatRatePercent)).toEqual([27, 5, 0]);
  });

  // S159 — the composer carries each line's `unit` (stamped by the
  // PR-100 product picker) onto the wire `lines[i].unit`, where the
  // backend threads it to the NAV `<unitOfMeasure>` emit. A freetext
  // line (no product picked) emits `unit: null` → PIECE fallback.
  describe("S159 — line.unit carries to the wire body", () => {
    it("carries a Nav unit (LITER) verbatim", () => {
      const body = composeIssueInvoiceBody({
        ...emptyForm(),
        customerName: "C",
        customerTaxNumber: "y",
        lines: [
          {
            description: "Áram",
            quantityInput: "2",
            unitPriceInput: "1000",
            vatRatePercent: 27,
            vatRateKind: "Percent" as const,
            note: "",
            unit: { kind: "Nav", value: "LITER" },
          },
        ],
      });
      expect(body.lines[0].unit).toEqual({ kind: "Nav", value: "LITER" });
    });

    it("carries an Own unit (liter@15C) verbatim", () => {
      const body = composeIssueInvoiceBody({
        ...emptyForm(),
        customerName: "C",
        customerTaxNumber: "y",
        lines: [
          {
            description: "Áram",
            quantityInput: "2",
            unitPriceInput: "1000",
            vatRatePercent: 27,
            vatRateKind: "Percent" as const,
            note: "",
            unit: { kind: "Own", value: "liter@15C" },
          },
        ],
      });
      expect(body.lines[0].unit).toEqual({ kind: "Own", value: "liter@15C" });
    });

    it("emits unit: null for a one-off freetext line (no product picked)", () => {
      const body = composeIssueInvoiceBody({
        ...emptyForm(),
        customerName: "C",
        customerTaxNumber: "y",
        lines: [
          {
            description: "freetext",
            quantityInput: "1",
            unitPriceInput: "100",
            vatRatePercent: 27,
            vatRateKind: "Percent" as const,
            note: "",
          },
        ],
      });
      expect(body.lines[0].unit).toBeNull();
    });
  });

  // PR-97 / ADR-0048 — composer per buyer-kind branch.
  describe("PR-97 / ADR-0048 — customer.vatStatus", () => {
    it("emits vatStatus=Domestic from default form", () => {
      const body = composeIssueInvoiceBody({
        ...emptyForm(),
        customerName: "Vevő Kft.",
        customerTaxNumber: "87654321-2-13",
      });
      expect(body.customer.vatStatus).toBe("Domestic");
      expect(body.customer.taxNumber).toBe("87654321-2-13");
    });

    it("emits vatStatus=PrivatePerson + empty taxNumber when the operator picks a natural-person buyer", () => {
      const body = composeIssueInvoiceBody({
        ...emptyForm(),
        customerVatStatus: "PrivatePerson",
        customerName: "Kovács János",
        customerTaxNumber: "",
      });
      expect(body.customer.vatStatus).toBe("PrivatePerson");
      expect(body.customer.taxNumber).toBe("");
    });

    // Session-148 (Ervin override 3) — the buyer name is mandatory on
    // the invoice (§169) for ALL customer types. The composer must
    // carry `customer.name` for a PrivatePerson buyer; it must NEVER be
    // null/empty when the operator entered a name (the regression that
    // POSTed a null name and blocked issuance).
    it("carries customer.name for PrivatePerson (never null when entered)", () => {
      const body = composeIssueInvoiceBody({
        ...emptyForm(),
        customerVatStatus: "PrivatePerson",
        customerName: "Teszt Magánszemély",
        customerTaxNumber: "",
      });
      expect(body.customer.name).toBe("Teszt Magánszemély");
      expect(body.customer.name).not.toBe("");
    });

    it("emits vatStatus=Other + communityVatNumber for an Other (EU) buyer", () => {
      const body = composeIssueInvoiceBody({
        ...emptyForm(),
        customerVatStatus: "Other",
        customerName: "Foreign Buyer",
        customerTaxNumber: "",
        customerCommunityVatNumber: "ATU12345678",
      });
      // ADR-0102 — the composer snapshots the EU VAT number onto the
      // wire body for Other buyers; the backend preflight requires +
      // validates it.
      expect(body.customer.vatStatus).toBe("Other");
      expect(body.customer.communityVatNumber).toBe("ATU12345678");
    });

    it("omits communityVatNumber for a non-Other buyer", () => {
      const body = composeIssueInvoiceBody({
        ...emptyForm(),
        customerVatStatus: "Domestic",
        customerCommunityVatNumber: "ATU12345678",
      });
      // ADR-0102 — the number rides on the wire ONLY for Other buyers,
      // so a Domestic body stays byte-identical (undefined → serde None).
      expect(body.customer.communityVatNumber).toBeUndefined();
    });
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
    // Session-150 — §169 buyer-address gate. Previously missing from
    // the front-end closed-vocab; a backend `CustomerAddressMissing`
    // collapsed the whole parse to null and no chip rendered.
    {
      kind: "CustomerAddressMissing",
      field_path: "customer.address",
      message_hu:
        "A vevő címe kötelező a számlán (Áfa tv. §169) — pótold a partner adatlapján (ország, irányítószám, város, utca).",
      message_en:
        "Buyer address required per §169 — fix the partner record (country, postal code, city, street).",
    },
    // PR-97 / ADR-0048 — reachable via the PrivatePerson / foreign-buyer
    // paths; added to the front-end vocab alongside CustomerAddressMissing.
    {
      kind: "CustomerTaxNumberPresentForPrivatePerson",
      field_path: "customer.taxNumber",
      message_hu:
        "Magánszemély vevőhöz nem tartozhat adószám (kapott: `12345678-2-13`).",
      message_en:
        "A private-person buyer must not carry a tax number (got `12345678-2-13`).",
    },
    // ADR-0102 — EU-partner (Other) customer type + cross-field matrix.
    {
      kind: "CommunityVatNumberMissing",
      field_path: "customer.communityVatNumber",
      message_hu: "Külföldi (OTHER) vevőhöz kötelező az EU adószám.",
      message_en: "Foreign-EU (OTHER) buyers require an EU VAT number.",
    },
    {
      kind: "VatKindRequiresOtherBuyer",
      field_path: "customer.vatStatus",
      message_hu: "A(z) 1. sor ÁFA-típusa külföldi EU-s vevőt igényel.",
      message_en: "Line 1 VAT type requires a foreign-EU buyer.",
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
      message_hu: "A(z) 1. tételsor mennyisége nullánál nagyobb kell legyen.",
      message_en: "Line 1 quantity must be greater than zero.",
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
    // ADR-0101 (Session 2) — the three per-line VAT rate-kind variants. All
    // route to `lines[N].vatRateKind`. Adding them here proves
    // `isKnownPreflightKind` recognises them (a regression that dropped one
    // would surface as `null` from the parser).
    {
      kind: "NonZeroPercentForExemptKind",
      field_path: "lines[0].vatRateKind",
      message_hu:
        "A(z) 1. tételsor ÁFA-típusa (AamExempt) adómentes / fordított adózású, ezért az ÁFA-kulcs kötelezően 0% (kapott: 27%).",
      message_en:
        "Line 1 VAT kind (AamExempt) is exempt / reverse-charge, so its VAT rate must be 0% (got 27%).",
    },
    {
      kind: "VatRateKindNotSupportedYet",
      field_path: "lines[0].vatRateKind",
      message_hu:
        "A(z) 1. tételsor ÁFA-típusa (TamExempt) még nincs bekötve (ADR-0101 named-deferred).",
      message_en:
        "Line 1 VAT kind (TamExempt) is not wired yet (ADR-0101 named-deferred).",
    },
    {
      kind: "MixedVatRateKindsUnsupported",
      field_path: "lines[1].vatRateKind",
      message_hu:
        "A(z) 2. tételsor ÁFA-típusa (AamExempt) eltér a számla első tételsorának típusától (Percent).",
      message_en:
        "Line 2 VAT kind (AamExempt) differs from the invoice's first line kind (Percent).",
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

  // Session-150 — end-to-end: a blocked issuance whose preflight
  // returns the §169 buyer-address error parses into the typed body,
  // routes to the address group, and carries the §169 citation in BOTH
  // languages so the operator-facing chip is bilingual and actionable.
  it("surfaces the §169 buyer-address chip with a bilingual message", () => {
    const raw = preflightBodyJson([
      {
        kind: "CustomerAddressMissing",
        field_path: "customer.address",
        message_hu:
          "A vevő címe kötelező a számlán (Áfa tv. §169) — pótold a partner adatlapján (ország, irányítószám, város, utca).",
        message_en:
          "Buyer address required per §169 — fix the partner record (country, postal code, city, street).",
      },
    ]);
    const parsed = parseInvoicePreflightErrors(raw);
    expect(parsed).not.toBeNull();
    const item = parsed!.errors[0];
    expect(item.kind).toBe("CustomerAddressMissing");
    expect(targetForFieldPath(item.field_path)).toEqual({
      kind: "customer",
      field: "address",
    });
    expect(item.message_hu).toContain("§169");
    expect(item.message_en).toContain("§169");
  });

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

  // Session-150 — the §169 buyer-address error routes to the address
  // field group so it renders inline (bilingual) rather than dropping
  // into the HU-only unrouted catch-all.
  it("routes customer.address to the customer-address group", () => {
    expect(targetForFieldPath("customer.address")).toEqual({
      kind: "customer",
      field: "address",
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
    // ADR-0101 — the per-line VAT-type selector path the three kind-level
    // preflight errors route to. The Session-1 flag was that the discriminant
    // union / router did NOT know this path; this pin proves it now does.
    expect(targetForFieldPath("lines[5].vatRateKind")).toEqual({
      kind: "line",
      lineIndex: 5,
      field: "vatRateKind",
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

  // ADR-0102 — EU community VAT input + buyer-type control routing.
  it("routes the ADR-0102 customer paths", () => {
    expect(targetForFieldPath("customer.communityVatNumber")).toEqual({
      kind: "customer",
      field: "communityVatNumber",
    });
    expect(targetForFieldPath("customer.vatStatus")).toEqual({
      kind: "customer",
      field: "vatStatus",
    });
  });
});

// ADR-0102 — cross-field kind↔buyer guidance helper.
describe("vatKindBuyerMismatch — cross-field guidance", () => {
  it("flags an EU-0 kind against a non-Other buyer", () => {
    expect(vatKindBuyerMismatch("IntraCommunityGoods", "Domestic")).not.toBeNull();
    expect(
      vatKindBuyerMismatch("IntraCommunityServiceReverse", "PrivatePerson"),
    ).not.toBeNull();
    // ...and clears once the buyer is Other.
    expect(vatKindBuyerMismatch("IntraCommunityGoods", "Other")).toBeNull();
  });

  it("flags a domestic kind against a non-Domestic buyer", () => {
    expect(vatKindBuyerMismatch("AamExempt", "Other")).not.toBeNull();
    expect(vatKindBuyerMismatch("DomesticReverseCharge", "Other")).not.toBeNull();
    expect(vatKindBuyerMismatch("AamExempt", "Domestic")).toBeNull();
  });

  it("never flags Percent (buyer-agnostic)", () => {
    for (const s of ["Domestic", "PrivatePerson", "Other"] as const) {
      expect(vatKindBuyerMismatch("Percent", s)).toBeNull();
    }
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

// ──────────────────────────────────────────────────────────────────────
// PR-84 — invoice-date composer pins
// ──────────────────────────────────────────────────────────────────────

describe("paymentDeadlineFromOffset", () => {
  it("returns invoiceDate + offsetDays as a YYYY-MM-DD string", () => {
    expect(paymentDeadlineFromOffset("2026-05-27", 8)).toBe("2026-06-04");
    expect(paymentDeadlineFromOffset("2026-05-27", 0)).toBe("2026-05-27");
    expect(paymentDeadlineFromOffset("2026-05-27", 30)).toBe("2026-06-26");
  });

  it("handles month and year boundaries", () => {
    expect(paymentDeadlineFromOffset("2026-12-25", 10)).toBe("2027-01-04");
  });

  it("returns null on malformed input", () => {
    expect(paymentDeadlineFromOffset("not-a-date", 8)).toBeNull();
    expect(paymentDeadlineFromOffset("2026-05-27", Number.NaN)).toBeNull();
  });
});

describe("deliveryDateOverrideFor", () => {
  // The form uses this helper to decide whether to fire the inline
  // "Are you sure?" confirm — and to stamp the audit discriminant on
  // the wire body. The three pins below cover the three closed-vocab
  // outcomes; the underlying boundary semantics are pinned in
  // `invoice-dates.test.ts`'s comfortZone block.

  it("returns null for an in-range delivery date (no audit flag)", () => {
    expect(deliveryDateOverrideFor("2026-05-27", "2026-06-04", "2026-05-30")).toBeNull();
  });

  it("returns null at either inclusive endpoint", () => {
    expect(deliveryDateOverrideFor("2026-05-27", "2026-06-04", "2026-05-27")).toBeNull();
    expect(deliveryDateOverrideFor("2026-05-27", "2026-06-04", "2026-06-04")).toBeNull();
  });

  it("returns 'BeforeInvoiceDate' for back-dated delivery", () => {
    expect(deliveryDateOverrideFor("2026-05-27", "2026-06-04", "2026-05-26")).toBe(
      "BeforeInvoiceDate",
    );
  });

  it("returns 'AfterPaymentDeadline' for forward-dated delivery", () => {
    expect(deliveryDateOverrideFor("2026-05-27", "2026-06-04", "2026-06-05")).toBe(
      "AfterPaymentDeadline",
    );
  });

  it("returns null on a malformed range (form-level validator catches separately)", () => {
    // payment_deadline < invoice_date — the form's input validator
    // should already surface the problem; the override helper refuses
    // to classify against a malformed range rather than producing a
    // misleading confirm prompt.
    expect(deliveryDateOverrideFor("2026-06-04", "2026-05-27", "2026-05-30")).toBeNull();
  });
});

describe("composeIssueInvoiceBody — PR-84 invoice-date fields", () => {
  it("emits paymentDeadline and deliveryDate verbatim from the form", () => {
    const form = {
      ...emptyForm(),
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      invoiceDate: "2026-05-27",
      paymentDeadline: "2026-06-04",
      deliveryDate: "2026-05-30",
      deliveryDateOverride: null,
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.paymentDeadline).toBe("2026-06-04");
    expect(body.deliveryDate).toBe("2026-05-30");
    expect(body.deliveryDateOverride).toBeNull();
  });

  it("emits a non-null deliveryDateOverride verbatim (operator confirmed out-of-range)", () => {
    const form = {
      ...emptyForm(),
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      invoiceDate: "2026-05-27",
      paymentDeadline: "2026-06-04",
      deliveryDate: "2026-05-10",
      deliveryDateOverride: "BeforeInvoiceDate" as const,
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.deliveryDate).toBe("2026-05-10");
    expect(body.deliveryDateOverride).toBe("BeforeInvoiceDate");
  });

  it("does NOT carry the form's invoiceDate on the wire (server stamps the issue date)", () => {
    // Per ADR-0007 §"Operator-as-threat-actor" the server stamps the
    // immutable issue date. The form's `invoiceDate` is purely UX
    // anchoring for the two operator-supplied dates; emitting it on
    // the wire would be a regression that lets the client clock
    // influence the regulatory record.
    const form = {
      ...emptyForm(),
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      invoiceDate: "1999-01-01", // operator-typed lie
    };
    const body = composeIssueInvoiceBody(form);
    expect(body).not.toHaveProperty("invoiceDate");
    expect(body).not.toHaveProperty("issueDate");
  });
});

// ──────────────────────────────────────────────────────────────────────
// S406 — product-pick currency auto-flip + derived mismatch warning
//
// Ervin's brief (2026-06-14): picking a product whose default currency
// differs from the invoice currency should AUTO-FLIP the invoice
// currency (and the bank picker) to the product's default, both still
// operator-overwritable; an override away from a line's product default
// then surfaces a code-derived warning chip. These pins live entirely
// on the pure helpers so the flip + warning are verified without
// mounting IssueInvoice.svelte.
// ──────────────────────────────────────────────────────────────────────

function makeProduct(overrides: Partial<Product> = {}): Product {
  return {
    id: "prd_0000000000000000000000EUR",
    name: "Konzultáció",
    unit: { kind: "Nav", value: "HOUR" },
    currency: "EUR",
    // 340,50 EUR = 34050 cents.
    unit_price_minor: 34050,
    created_at: "2026-06-14T00:00:00Z",
    updated_at: "2026-06-14T00:00:00Z",
    deleted_at: null,
    ...overrides,
  };
}

const HUF_BANK: SellerBankResponse = {
  id: "bnk_huf_0000000000000000000001",
  currency: "HUF",
  account_number: "12345678-12345678-12345678",
  bank_name: "OTP-HUF",
  swift_bic: "OTPVHUHB",
  is_default: true,
};

const EUR_BANK: SellerBankResponse = {
  id: "bnk_eur_0000000000000000000002",
  currency: "EUR",
  account_number: "HU42117730161111101800000000",
  bank_name: "UNICREDIT-EUR",
  swift_bic: "BACXHUHB",
  is_default: true,
};

const BANKS: SellerBankResponse[] = [HUF_BANK, EUR_BANK];

describe("applyProductPick — auto-flip currency on product selection", () => {
  it("flips the invoice currency to the product's default (EUR product, HUF invoice)", () => {
    const form = { ...emptyForm(), currency: "HUF" as const };
    expect(form.currency).toBe("HUF");

    const next = applyProductPick(form, 0, makeProduct({ currency: "EUR" }));

    // Currency flipped to the product's default.
    expect(next.currency).toBe("EUR");
    // Line autofilled: description, price (formatted in EUR), unit, stamp.
    expect(next.lines[0].description).toBe("Konzultáció");
    expect(next.lines[0].unitPriceInput).toBe("340.50");
    expect(next.lines[0].productCurrency).toBe("EUR");
    expect(next.lines[0].unit).toEqual({ kind: "Nav", value: "HOUR" });
  });

  it("is idempotent when the product currency already matches", () => {
    const form = { ...emptyForm(), currency: "HUF" as const };
    const next = applyProductPick(
      form,
      0,
      makeProduct({ currency: "HUF", unit_price_minor: 340000, name: "Acél" }),
    );
    expect(next.currency).toBe("HUF");
    expect(next.lines[0].productCurrency).toBe("HUF");
    expect(next.lines[0].unitPriceInput).toBe("340000");
  });

  it("only touches the picked line; sibling lines are untouched", () => {
    const form = {
      ...emptyForm(),
      currency: "HUF" as const,
      lines: [
        { ...emptyForm().lines[0], description: "Existing", unitPriceInput: "100" },
        { ...emptyForm().lines[0] },
      ],
    };
    const next = applyProductPick(form, 1, makeProduct({ currency: "EUR" }));
    expect(next.lines[0].description).toBe("Existing");
    expect(next.lines[0].unitPriceInput).toBe("100");
    expect(next.lines[1].description).toBe("Konzultáció");
  });
});

describe("resolveBankForCurrency — bank picker re-defaults with the flip", () => {
  it("flips to the EUR default bank when the invoice flips to EUR", () => {
    // Operator had the HUF default bank; picking a EUR product flips the
    // currency, and the bank picker re-defaults to the EUR bank.
    const resolved = resolveBankForCurrency(BANKS, "EUR", HUF_BANK.id);
    expect(resolved).toBe(EUR_BANK.id);
  });

  it("flips back to the HUF default bank on a HUF override", () => {
    const resolved = resolveBankForCurrency(BANKS, "HUF", EUR_BANK.id);
    expect(resolved).toBe(HUF_BANK.id);
  });

  it("preserves an operator's explicit pick that already matches the currency", () => {
    // A second EUR account the operator chose by hand; resolving for EUR
    // must NOT yank it back to the default.
    const secondEur: SellerBankResponse = {
      ...EUR_BANK,
      id: "bnk_eur_0000000000000000000003",
      bank_name: "REVOLUT-EUR",
      is_default: false,
    };
    const banks = [HUF_BANK, EUR_BANK, secondEur];
    const resolved = resolveBankForCurrency(banks, "EUR", secondEur.id);
    expect(resolved).toBe(secondEur.id);
  });

  it("returns null when no bank is configured for the currency", () => {
    // HUF-only tenant flips to EUR: no EUR bank → blank the picker (the
    // form renders the no-bank-for-currency affordance + blocks Submit).
    const resolved = resolveBankForCurrency([HUF_BANK], "EUR", HUF_BANK.id);
    expect(resolved).toBeNull();
  });
});

describe("lineCurrencyMismatchWarning — code-derived override warning", () => {
  it("returns null at pick time (currency was flipped to match)", () => {
    // After applyProductPick the invoice currency equals the product's,
    // so the line shows no warning.
    expect(
      lineCurrencyMismatchWarning({
        productCurrency: "EUR",
        invoiceCurrency: "EUR",
        productName: "Konzultáció",
        banks: BANKS,
      }),
    ).toBeNull();
  });

  it("returns null for a one-off line (no product picked)", () => {
    expect(
      lineCurrencyMismatchWarning({
        productCurrency: null,
        invoiceCurrency: "HUF",
        productName: "",
        banks: BANKS,
      }),
    ).toBeNull();
    expect(
      lineCurrencyMismatchWarning({
        productCurrency: undefined,
        invoiceCurrency: "HUF",
        productName: "",
        banks: BANKS,
      }),
    ).toBeNull();
  });

  it("warns + names the product-currency bank when the operator overrides to HUF", () => {
    // The override path: operator flips the EUR-product invoice back to
    // HUF. The chip names the product, both currencies, and the EUR bank
    // that "may not apply." Derived from the wire shape — never operator
    // memory ([[trust-code-not-operator]]).
    const warn = lineCurrencyMismatchWarning({
      productCurrency: "EUR",
      invoiceCurrency: "HUF",
      productName: "Konzultáció",
      banks: BANKS,
    });
    expect(warn).not.toBeNull();
    expect(warn!.productName).toBe("Konzultáció");
    expect(warn!.productCurrency).toBe("EUR");
    expect(warn!.invoiceCurrency).toBe("HUF");
    expect(warn!.productCurrencyBankName).toBe("UNICREDIT-EUR");
  });

  it("warns with a null bank name when no bank exists for the product currency", () => {
    const warn = lineCurrencyMismatchWarning({
      productCurrency: "EUR",
      invoiceCurrency: "HUF",
      productName: "Konzultáció",
      banks: [HUF_BANK], // no EUR bank configured
    });
    expect(warn).not.toBeNull();
    expect(warn!.productCurrencyBankName).toBeNull();
  });
});

describe("S406 journey — pick EUR product, override to HUF, compose", () => {
  it("flips on pick, re-defaults the bank, then warns on override and issues with the override currency", () => {
    // 1. Operator starts on a HUF invoice with the HUF default bank.
    let form: IssueInvoiceFormState = {
      ...emptyForm(),
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      currency: "HUF",
      bankAccountId: HUF_BANK.id,
    };

    // 2. Picks a EUR product → currency + bank flip to EUR; no warning.
    form = applyProductPick(form, 0, makeProduct({ currency: "EUR" }));
    form = {
      ...form,
      bankAccountId: resolveBankForCurrency(BANKS, form.currency, form.bankAccountId),
    };
    expect(form.currency).toBe("EUR");
    expect(form.bankAccountId).toBe(EUR_BANK.id);
    expect(
      lineCurrencyMismatchWarning({
        productCurrency: form.lines[0].productCurrency,
        invoiceCurrency: form.currency,
        productName: form.lines[0].description,
        banks: BANKS,
      }),
    ).toBeNull();

    // 3. Operator overrides the currency back to HUF (overwritable). The
    //    bank re-defaults to HUF and the mismatch chip now fires.
    form = { ...form, currency: "HUF" as const };
    form = {
      ...form,
      bankAccountId: resolveBankForCurrency(BANKS, form.currency, form.bankAccountId),
    };
    expect(form.bankAccountId).toBe(HUF_BANK.id);
    const warn = lineCurrencyMismatchWarning({
      productCurrency: form.lines[0].productCurrency,
      invoiceCurrency: form.currency,
      productName: form.lines[0].description,
      banks: BANKS,
    });
    expect(warn).not.toBeNull();
    expect(warn!.productCurrencyBankName).toBe("UNICREDIT-EUR");

    // 4. The invoice still composes cleanly with the override currency —
    //    the warning is advisory, not a hard block (the price the
    //    operator left under HUF rides verbatim per PR-88 semantics).
    const body = composeIssueInvoiceBody(form);
    expect(body.currency).toBe("HUF");
    expect(body.bankAccountId).toBe(HUF_BANK.id);
    expect(body.lines[0].description).toBe("Konzultáció");
    expect(body.lines[0].unit).toEqual({ kind: "Nav", value: "HOUR" });
  });
});

// ADR-0101 (Session 2) — the per-line VAT rate-kind selector + composer.
describe("ADR-0101 — VAT rate-kind selector", () => {
  const NON_PERCENT: VatRateKindBody[] = [
    "AamExempt",
    "DomesticReverseCharge",
    "IntraCommunityGoods",
    "IntraCommunityServiceReverse",
  ];

  it("VAT_KIND_OPTIONS exposes exactly the five selectable kinds, Percent first", () => {
    expect(VAT_KIND_OPTIONS.map((o) => o.value)).toEqual([
      "Percent",
      "AamExempt",
      "DomesticReverseCharge",
      "IntraCommunityGoods",
      "IntraCommunityServiceReverse",
    ]);
    // Every option carries a bilingual label + hint (HÜLYE-BIZTOS — the
    // operator never sees a bare code with no explanation).
    for (const o of VAT_KIND_OPTIONS) {
      expect(o.labelHu.length).toBeGreaterThan(0);
      expect(o.labelEn.length).toBeGreaterThan(0);
      expect(o.hintHu.length).toBeGreaterThan(0);
      expect(o.hintEn.length).toBeGreaterThan(0);
    }
  });

  it("vatKindLocksRate is true for every non-Percent kind, false for Percent", () => {
    expect(vatKindLocksRate("Percent")).toBe(false);
    for (const k of NON_PERCENT) {
      expect(vatKindLocksRate(k)).toBe(true);
    }
  });

  it("applyVatKind locks the rate to 0 for a non-Percent kind", () => {
    for (const k of NON_PERCENT) {
      // Start from a 27% line, pick the exempt/reverse kind → rate forced 0.
      const form = emptyForm(); // one line, vatRatePercent 27, kind Percent
      const next = applyVatKind(form, 0, k);
      expect(next.lines[0].vatRateKind).toBe(k);
      expect(next.lines[0].vatRatePercent).toBe(0);
      // Purity — the input form is untouched.
      expect(form.lines[0].vatRateKind).toBe("Percent");
      expect(form.lines[0].vatRatePercent).toBe(27);
    }
  });

  it("applyVatKind restores the standard 27% when switching back to Percent from a locked 0", () => {
    let form = emptyForm();
    form = applyVatKind(form, 0, "AamExempt"); // rate now locked to 0
    expect(form.lines[0].vatRatePercent).toBe(0);
    form = applyVatKind(form, 0, "Percent"); // unlock — restore 27
    expect(form.lines[0].vatRateKind).toBe("Percent");
    expect(form.lines[0].vatRatePercent).toBe(27);
  });

  it("applyVatKind preserves a non-zero Percent rate when re-selecting Percent", () => {
    // A Percent line the operator set to 5% must not be bumped to 27 by a
    // no-op Percent re-select.
    let form = emptyForm();
    form = { ...form, lines: [{ ...form.lines[0], vatRatePercent: 5 }] };
    form = applyVatKind(form, 0, "Percent");
    expect(form.lines[0].vatRatePercent).toBe(5);
  });

  it("applyVatKind only touches the targeted line", () => {
    let form = emptyForm();
    form = { ...form, lines: [form.lines[0], { ...emptyForm().lines[0] }] };
    form = applyVatKind(form, 1, "DomesticReverseCharge");
    expect(form.lines[0].vatRateKind).toBe("Percent");
    expect(form.lines[0].vatRatePercent).toBe(27);
    expect(form.lines[1].vatRateKind).toBe("DomesticReverseCharge");
    expect(form.lines[1].vatRatePercent).toBe(0);
  });

  it("composeIssueInvoiceBody emits vatRateKind and forces rate 0 for non-Percent", () => {
    for (const k of NON_PERCENT) {
      // Even if a stale non-zero rate slipped past the UI lock, the composer
      // force-zeroes it (defence in depth).
      const form = {
        ...emptyForm(),
        lines: [
          {
            ...emptyForm().lines[0],
            description: "svc",
            unitPriceInput: "1000",
            vatRateKind: k,
            vatRatePercent: 27, // deliberately wrong — must be zeroed
          },
        ],
      };
      const body = composeIssueInvoiceBody(form);
      expect(body.lines[0].vatRateKind).toBe(k);
      expect(body.lines[0].vatRatePercent).toBe(0);
    }
  });

  it("composeIssueInvoiceBody leaves a Percent line's rate untouched (backward-compat)", () => {
    const form = {
      ...emptyForm(),
      lines: [
        {
          ...emptyForm().lines[0],
          description: "svc",
          unitPriceInput: "1000",
          vatRateKind: "Percent" as const,
          vatRatePercent: 27,
        },
      ],
    };
    const body = composeIssueInvoiceBody(form);
    expect(body.lines[0].vatRateKind).toBe("Percent");
    expect(body.lines[0].vatRatePercent).toBe(27);
  });
});
