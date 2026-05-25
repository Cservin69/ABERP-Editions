// PR-47β / session-65 — vitest pin for the Modification form-to-
// request-body composer + the pre-fill `formFromIssuanceInput` seam.
//
// The composer is the load-bearing seam between the operator-edited
// form state and the wire `ModificationInvoiceRequest` shape. The
// pre-fill seam is the operator-visible affordance that turns "open
// modification modal" from "retype the entire invoice" into "edit
// in place" — a regression that mis-maps the side-stored
// `IssueInvoiceRequest` into the form state would silently lose
// content (e.g., a renamed `unitPrice` field would leave the line
// at 0 and the operator would only notice on the printed invoice).
// CLAUDE.md rule 9: per-field assertions so a regression that
// collapses every line item to a constant cannot pass vacuously.
//
// Mirror invariant per A156 / A161: the backend's
// `serve::ModificationInvoiceRequest` Deserialize and this composer
// agree on the wire field names (camelCase JSON, UPPERCASE currency,
// `modificationDate` per ADR-0024 §1).

import { describe, expect, it } from "vitest";

import type { IssueInvoiceRequest } from "./api";
import {
  composeModificationBody,
  emptyModificationForm,
  formFromIssuanceInput,
} from "./modification";

describe("composeModificationBody", () => {
  it("reshapes form state into the wire body with modificationDate", () => {
    const form = {
      ...emptyModificationForm("HUF"),
      supplierName: "ABERP Supplier Kft.",
      supplierTaxNumber: "12345678-1-42",
      supplierCountryCode: "HU",
      supplierPostalCode: "1011",
      supplierCity: "Budapest",
      supplierStreet: "Fő utca 1.",
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      lines: [
        {
          description: "Corrected widget A",
          quantity: 3,
          unitPriceMinor: 1200,
          vatRatePercent: 27,
        },
      ],
      modificationDate: "2026-05-24",
    };
    const body = composeModificationBody(form);
    expect(body).toEqual({
      supplier: {
        taxNumber: "12345678-1-42",
        name: "ABERP Supplier Kft.",
        address: {
          countryCode: "HU",
          postalCode: "1011",
          city: "Budapest",
          street: "Fő utca 1.",
        },
      },
      customer: {
        taxNumber: "87654321-2-13",
        name: "Vevő Kft.",
      },
      lines: [
        {
          description: "Corrected widget A",
          quantity: 3,
          unitPrice: 1200,
          vatRatePercent: 27,
        },
      ],
      currency: "HUF",
      modificationDate: "2026-05-24",
    });
  });

  it("trims whitespace on every string field including modificationDate", () => {
    // Defence in depth — the backend's date validator only accepts
    // canonical YYYY-MM-DD; surrounding whitespace would silently
    // produce a 400. Trim here so the operator sees the error only
    // when they actually typed a malformed date.
    const form = {
      ...emptyModificationForm("EUR"),
      supplierName: "  Trimmed supplier  ",
      supplierTaxNumber: "  12345678-1-42  ",
      customerName: "  Trimmed buyer  ",
      modificationDate: "  2026-05-24  ",
      lines: [
        {
          description: "  trimmed desc  ",
          quantity: 1,
          unitPriceMinor: 100,
          vatRatePercent: 27,
        },
      ],
    };
    const body = composeModificationBody(form);
    expect(body.supplier.name).toBe("Trimmed supplier");
    expect(body.supplier.taxNumber).toBe("12345678-1-42");
    expect(body.customer.name).toBe("Trimmed buyer");
    expect(body.modificationDate).toBe("2026-05-24");
    expect(body.lines[0].description).toBe("trimmed desc");
  });

  it("propagates currency verbatim (HUF and EUR)", () => {
    // The form's currency is locked to the base's currency at the
    // <select disabled> layer; the composer is the second line of
    // defence — it does NOT silently coerce to HUF.
    for (const currency of ["HUF", "EUR"] as const) {
      const form = emptyModificationForm(currency);
      const body = composeModificationBody(form);
      expect(body.currency).toBe(currency);
    }
  });
});

describe("formFromIssuanceInput", () => {
  it("maps side-stored issuance input into the modification form state", () => {
    // The side-stored `<ULID>.input.json` carries the
    // `IssueInvoiceRequest` shape; the modification form's state
    // shape uses snake-case-free field names + `unitPriceMinor`. The
    // mapper is the seam — a renamed field would silently strand the
    // operator's content. Per-field assertions per CLAUDE.md rule 9.
    const input: IssueInvoiceRequest = {
      supplier: {
        taxNumber: "12345678-1-42",
        name: "ABERP Supplier Kft.",
        address: {
          countryCode: "HU",
          postalCode: "1011",
          city: "Budapest",
          street: "Fő utca 1.",
        },
      },
      customer: {
        taxNumber: "87654321-2-13",
        name: "Vevő Kft.",
      },
      lines: [
        {
          description: "Widget A",
          quantity: 2,
          unitPrice: 1000,
          vatRatePercent: 27,
        },
        {
          description: "Widget B",
          quantity: 1,
          unitPrice: 5000,
          vatRatePercent: 5,
        },
      ],
      currency: "EUR",
    };
    const form = formFromIssuanceInput(input, "EUR");
    expect(form.supplierTaxNumber).toBe("12345678-1-42");
    expect(form.supplierName).toBe("ABERP Supplier Kft.");
    expect(form.supplierCountryCode).toBe("HU");
    expect(form.supplierPostalCode).toBe("1011");
    expect(form.supplierCity).toBe("Budapest");
    expect(form.supplierStreet).toBe("Fő utca 1.");
    expect(form.customerTaxNumber).toBe("87654321-2-13");
    expect(form.customerName).toBe("Vevő Kft.");
    expect(form.currency).toBe("EUR");
    expect(form.lines).toEqual([
      {
        description: "Widget A",
        quantity: 2,
        unitPriceMinor: 1000,
        vatRatePercent: 27,
      },
      {
        description: "Widget B",
        quantity: 1,
        unitPriceMinor: 5000,
        vatRatePercent: 5,
      },
    ]);
    // modificationDate defaults to today; the operator can overwrite.
    // We pin the canonical YYYY-MM-DD shape — content varies by clock.
    expect(form.modificationDate).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  });

  it("sources currency from the baseCurrency arg, not the input body", () => {
    // C6 invariant — the side-stored body's currency MAY be stale (a
    // hand-edited input.json could carry a different currency than
    // the billing row). The base's currency is the source of truth.
    // The mapper takes both inputs and emits the form with the
    // billing-row-sourced currency.
    const input: IssueInvoiceRequest = {
      supplier: {
        taxNumber: "12345678-1-42",
        name: "S",
        address: { countryCode: "HU", postalCode: "1", city: "B", street: "F" },
      },
      customer: { taxNumber: "87654321-2-13", name: "C" },
      lines: [
        {
          description: "L",
          quantity: 1,
          unitPrice: 1,
          vatRatePercent: 27,
        },
      ],
      currency: "HUF", // body says HUF…
    };
    const form = formFromIssuanceInput(input, "EUR"); // …but base says EUR.
    expect(form.currency).toBe("EUR");
  });
});
