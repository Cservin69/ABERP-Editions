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
// S174 / PR-174 — Modify reuses Issue's preflight parse + field-router
// (the backend's 400 body shape is identical across the issue and
// modification HTTP routes). These pins guard the shared contract for
// the Modify form's new inline-error surface; the Svelte component
// itself is svelte-check verified.
import {
  parseInvoicePreflightErrors,
  targetForFieldPath,
} from "./issue-invoice";

describe("composeModificationBody", () => {
  it("reshapes form state into the wire body with modificationDate", () => {
    const form = {
      ...emptyModificationForm("HUF"),
      customerName: "Vevő Kft.",
      customerTaxNumber: "87654321-2-13",
      lines: [
        {
          description: "Corrected widget A",
          quantityInput: "3",
          // PR-88 / session-113 — operator-typed string parsed at
          // compose time. HUF 0-decimal so `"1200"` → 1200 minor =
          // 1200 forints (same wire output as pre-PR-88 `unitPriceMinor: 1200`).
          unitPriceInput: "1200",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          note: "",
        },
      ],
      modificationDate: "2026-05-24",
    };
    const body = composeModificationBody(form);
    expect(body).toEqual({
      customer: {
        // PR-97 / ADR-0048 — composer emits the buyer-kind discriminator
        // inherited from the base via `formFromIssuanceInput`.
        vatStatus: "Domestic",
        taxNumber: "87654321-2-13",
        name: "Vevő Kft.",
        // PR-77 — `address: undefined` when the form's address quartet is blank.
        address: undefined,
      },
      lines: [
        {
          description: "Corrected widget A",
          quantity: "3",
          unitPrice: 1200,
          vatRatePercent: 27,
          // ADR-0101 — the ModificationInvoiceRequest wire body carries NO
          // vatRateKind (the modification wire path does not thread the kind
          // — see modification.ts FLAG); composeModificationBody must NOT
          // emit it.
          // PR-82 — blank-after-trim ⇒ null on the wire.
          note: null,
        },
      ],
      currency: "HUF",
      modificationDate: "2026-05-24",
      // PR-203 / S203 — empty form has no override; composer emits null.
      emailRecipientOverride: null,
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
          quantityInput: "1",
          // PR-88 / session-113 — operator-typed EUR amount. `"1"`
          // parses to 100 cents (= 1.00 EUR). The trim assertions
          // below don't check unitPrice so any non-zero amount works
          // here; using `"1"` makes the round-trip obvious.
          unitPriceInput: "1",
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
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
          quantity: "2",
          unitPrice: 1000,
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          note: "",
        },
        {
          description: "Widget B",
          quantity: "1",
          unitPrice: 5000,
          vatRatePercent: 5,
          vatRateKind: "Percent" as const,
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
        quantityInput: "2",
        unitPriceInput: "10.00",
        vatRatePercent: 27,
        vatRateKind: "Percent" as const,
        note: "",
      },
      {
        description: "Widget B",
        quantityInput: "1",
        unitPriceInput: "50.00",
        vatRatePercent: 5,
        vatRateKind: "Percent" as const,
        note: "",
      },
    ]);
    // modificationDate defaults to today; the operator can overwrite.
    // We pin the canonical YYYY-MM-DD shape — content varies by clock.
    expect(form.modificationDate).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  });

  it("populates the form with the base's customer fields so the operator edits in place", () => {
    // S174 / PR-174 — explicit edit-in-place pin per the brief
    // ("populating from the base invoice's current state so operator
    // sees the existing values and edits in place"). The mapper
    // already carries customer + address fields verbatim from the
    // side-stored issuance input; this test names the contract so a
    // future refactor that drops a field would fail loud rather than
    // silently blank the modify-form pre-fill.
    const input: IssueInvoiceRequest = {
      customer: {
        taxNumber: "12345678-1-42",
        name: "Régi Vevő Kft.",
        vatStatus: "Domestic",
        address: {
          countryCode: "HU",
          postalCode: "1051",
          city: "Budapest",
          street: "Andrássy út 1.",
        },
      },
      lines: [
        {
          description: "Inherited line",
          quantity: "1",
          unitPrice: 1234,
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          note: "",
        },
      ],
      currency: "HUF",
    };
    const form = formFromIssuanceInput(input, "HUF");
    expect(form.customerName).toBe("Régi Vevő Kft.");
    expect(form.customerTaxNumber).toBe("12345678-1-42");
    expect(form.customerVatStatus).toBe("Domestic");
    expect(form.customerCountryCode).toBe("HU");
    expect(form.customerPostalCode).toBe("1051");
    expect(form.customerCity).toBe("Budapest");
    expect(form.customerStreet).toBe("Andrássy út 1.");
    // Lines carry the operator's prior typed strings round-tripped
    // through the format helpers (HUF unit prices stay as bare ints).
    expect(form.lines[0].description).toBe("Inherited line");
    expect(form.lines[0].quantityInput).toBe("1");
    expect(form.lines[0].unitPriceInput).toBe("1234");
    expect(form.lines[0].vatRatePercent).toBe(27);
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
          quantity: "1",
          unitPrice: 1,
          vatRatePercent: 27,
          vatRateKind: "Percent" as const,
          note: "",
        },
      ],
      currency: "HUF", // body says HUF…
    };
    const form = formFromIssuanceInput(input, "EUR"); // …but base says EUR.
    expect(form.currency).toBe("EUR");
  });
});

// S174 / PR-174 — pin that the Modify form's preflight error routing
// shares the same parse + field-mapping contract IssueInvoice uses.
// The Svelte component itself (ModificationInvoice.svelte) wires its
// own per-field state buckets; this suite proves the parser the
// component delegates to handles the modification-route's 400-body
// shape unchanged from the issue-route's shape (the backend's
// `validate_invoice_preflight` is shared across both — same closed
// vocab, same field_path grammar). A regression here would surface
// as a `null` parse + the form silently dropping into the raw error
// string instead of inline-routing — exactly the CLAUDE.md rule 12
// failure mode the inline surface is designed to avoid.
describe("Modify form preflight inline-error routing", () => {
  function preflightWire(
    items: Array<{
      kind: string;
      field_path: string;
      message_hu: string;
      message_en: string;
    }>,
  ) {
    return `backend returned 400 Bad Request for /api/invoices/inv_BASE/amend: ${JSON.stringify(
      {
        error: "invoice_preflight_failed",
        errors: items,
      },
    )}`;
  }

  it("parses the modification route's 400 body the same way as the issue route", () => {
    const raw = preflightWire([
      {
        kind: "CustomerNameEmpty",
        field_path: "customer.name",
        message_hu: "A vevő neve kötelező.",
        message_en: "Customer name is required.",
      },
      {
        kind: "LineItemQuantityZero",
        field_path: "lines[0].quantity",
        message_hu: "A mennyiség nem lehet nulla.",
        message_en: "Line quantity must be greater than zero.",
      },
    ]);
    const parsed = parseInvoicePreflightErrors(raw);
    expect(parsed).not.toBeNull();
    expect(parsed!.errors).toHaveLength(2);
    expect(parsed!.errors[0].kind).toBe("CustomerNameEmpty");
    expect(parsed!.errors[1].kind).toBe("LineItemQuantityZero");
  });

  it("routes customer + line field paths to the same buckets IssueInvoice uses", () => {
    // The Modify form's `routePreflightErrors` (defined in the
    // Svelte component) calls into `targetForFieldPath` verbatim.
    // Pinning these representative targets here guards the contract
    // a future closed-vocab widening would silently break.
    expect(targetForFieldPath("customer.name")).toEqual({
      kind: "customer",
      field: "name",
    });
    expect(targetForFieldPath("customer.taxNumber")).toEqual({
      kind: "customer",
      field: "taxNumber",
    });
    expect(targetForFieldPath("customer.address")).toEqual({
      kind: "customer",
      field: "address",
    });
    expect(targetForFieldPath("lines")).toEqual({ kind: "lines" });
    expect(targetForFieldPath("lines[2].unitPrice")).toEqual({
      kind: "line",
      lineIndex: 2,
      field: "unitPrice",
    });
  });

  it("falls back to the raw error string when the body is not a preflight 400", () => {
    // Defence in depth — a 500 / network error / plain 400 (e.g.
    // `precondition_mismatch` from a stale base) must NOT collapse
    // the inline-error renderer; the component shows the raw message
    // in its general `.error` block. Pin: parser returns `null` for
    // any non-preflight shape.
    expect(parseInvoicePreflightErrors("network timeout")).toBeNull();
    expect(
      parseInvoicePreflightErrors(
        'backend returned 400 Bad Request for /api/invoices/inv_BASE/amend: {"error":"precondition_mismatch","detail":"base is annulled"}',
      ),
    ).toBeNull();
  });
});
