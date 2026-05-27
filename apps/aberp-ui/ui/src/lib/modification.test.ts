// PR-47β / session-65 — vitest pin for the Modification form-to-
// request-body composer + the pre-fill `formFromIssuanceInput` seam.
//
// PR-53 / session-73 — supplier fields removed from both the form
// shape and the wire shape. The composer + the pre-fill seam are
// pinned at customer + lines + currency + modificationDate only;
// supplier comes from seller.toml server-side now.
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
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      lines: [
        {
          description: "Corrected widget A",
          quantity: 3,
          // PR-88 / session-113 — operator-typed string parsed at
          // compose time. HUF 0-decimal so `"1200"` → 1200 minor =
          // 1200 forints (same wire output as pre-PR-88 `unitPriceMinor: 1200`).
          unitPriceInput: "1200",
          vatRatePercent: 27,
          note: "",
        },
      ],
      modificationDate: "2026-05-24",
    };
    const body = composeModificationBody(form);
    expect(body).toEqual({
      customer: {
        taxNumber: "87654321-2-13",
        name: "Vevő Kft.",
        // PR-77 — `address: undefined` when the form's address quartet is blank.
        address: undefined,
      },
      lines: [
        {
          description: "Corrected widget A",
          quantity: 3,
          unitPrice: 1200,
          vatRatePercent: 27,
          // PR-82 — blank-after-trim ⇒ null on the wire.
          note: null,
        },
      ],
      currency: "HUF",
      modificationDate: "2026-05-24",
    });
  });

  it("does not emit a supplier field on the wire body (PR-53)", () => {
    // Regression guard — the modification form parallels Issue in
    // dropping supplier; the wire body must NOT carry it.
    const form = emptyModificationForm("HUF");
    const body = composeModificationBody(form);
    expect("supplier" in body).toBe(false);
  });

  it("trims whitespace on every string field including modificationDate", () => {
    // Defence in depth — the backend's date validator only accepts
    // canonical YYYY-MM-DD; surrounding whitespace would silently
    // produce a 400. Trim here so the operator sees the error only
    // when they actually typed a malformed date.
    const form = {
      ...emptyModificationForm("EUR"),
      customerName: "  Trimmed buyer  ",
      modificationDate: "  2026-05-24  ",
      lines: [
        {
          description: "  trimmed desc  ",
          quantity: 1,
          // PR-88 / session-113 — operator-typed EUR amount. `"1"`
          // parses to 100 cents (= 1.00 EUR). The trim assertions
          // below don't check unitPrice so any non-zero amount works
          // here; using `"1"` makes the round-trip obvious.
          unitPriceInput: "1",
          vatRatePercent: 27,
          note: "",
        },
      ],
    };
    const body = composeModificationBody(form);
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
    // `IssueInvoiceRequest` shape (PR-53 dropped supplier from it);
    // the mapper folds customer + lines + currency into the form
    // state. Per-field assertions per CLAUDE.md rule 9.
    const input: IssueInvoiceRequest = {
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
          note: "",
        },
        {
          description: "Widget B",
          quantity: 1,
          unitPrice: 5000,
          vatRatePercent: 5,
          note: "",
        },
      ],
      currency: "EUR",
    };
    const form = formFromIssuanceInput(input, "EUR");
    expect(form.customerTaxNumber).toBe("87654321-2-13");
    expect(form.customerName).toBe("Vevő Kft.");
    expect(form.currency).toBe("EUR");
    // PR-88 / session-113 — the pre-fill mapper converts the
    // backend's integer minor-unit count back into the operator-
    // editable display string via `formatMinorToInput`. For EUR
    // (2-decimal) 1000 minor = "10.00" major; 5000 minor = "50.00"
    // major. The composer's round-trip re-produces the original
    // 1000 / 5000 minor on submit (pinned in format.test.ts).
    expect(form.lines).toEqual([
      {
        description: "Widget A",
        quantity: 2,
        unitPriceInput: "10.00",
        vatRatePercent: 27,
        note: "",
      },
      {
        description: "Widget B",
        quantity: 1,
        unitPriceInput: "50.00",
        vatRatePercent: 5,
        note: "",
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
      customer: { taxNumber: "87654321-2-13", name: "C" },
      lines: [
        {
          description: "L",
          quantity: 1,
          unitPrice: 1,
          vatRatePercent: 27,
          note: "",
        },
      ],
      currency: "HUF", // body says HUF…
    };
    const form = formFromIssuanceInput(input, "EUR"); // …but base says EUR.
    expect(form.currency).toBe("EUR");
  });
});
