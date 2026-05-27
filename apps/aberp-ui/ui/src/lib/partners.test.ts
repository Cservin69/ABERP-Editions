// PR-54 / session-74 — vitest pins for the partners helper module.
// Composer + mapper + filter + typed-error-body parser are pure
// functions (no Svelte, no Tauri); pinning them in isolation surfaces
// regressions before the dev-loop renders the form.
//
// Mirror invariant per A156 / A161 / A163: a backend drift that
// renames a field on `aberp::partners::Partner` /
// `aberp::partners::PartnerInputs` would surface here first via the
// snake_case wire-shape assertions.

import { describe, expect, it } from "vitest";

import type { Partner } from "./api";
import {
  buyerFieldsFromPartner,
  composePartnerInputs,
  emptyPartnerForm,
  filterPartners,
  formFromPartner,
  parsePartnerValidationError,
} from "./partners";

const SAMPLE_PARTNER: Partner = {
  id: "prt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
  display_name: "BSCE",
  legal_name: "Budapesti Sport-Egyesület Kft.",
  kind: "Customer",
  tax_number: "12345678-1-42",
  eu_vat_number: "HU12345678",
  address_street: "Fő utca 1.",
  address_postal_code: "1011",
  address_city: "Budapest",
  address_country: "Magyarország",
  bank_account: "12345678-12345678-12345678",
  contact_email: "ops@bsce.hu",
  contact_phone: "+36 1 234 5678",
  created_at: "2026-05-25T08:00:00Z",
  updated_at: "2026-05-25T08:00:00Z",
  deleted_at: null,
};

describe("emptyPartnerForm", () => {
  it("defaults kind=Customer and country=Magyarország per the brief", () => {
    const form = emptyPartnerForm();
    expect(form.kind).toBe("Customer");
    expect(form.addressCountry).toBe("Magyarország");
    expect(form.displayName).toBe("");
    expect(form.legalName).toBe("");
    expect(form.taxNumber).toBe("");
  });
});

describe("formFromPartner", () => {
  it("maps every populated field one-to-one", () => {
    const form = formFromPartner(SAMPLE_PARTNER);
    expect(form.displayName).toBe("BSCE");
    expect(form.legalName).toBe("Budapesti Sport-Egyesület Kft.");
    expect(form.kind).toBe("Customer");
    expect(form.taxNumber).toBe("12345678-1-42");
    expect(form.euVatNumber).toBe("HU12345678");
    expect(form.addressStreet).toBe("Fő utca 1.");
    expect(form.addressPostalCode).toBe("1011");
    expect(form.addressCity).toBe("Budapest");
    expect(form.addressCountry).toBe("Magyarország");
    expect(form.bankAccount).toBe("12345678-12345678-12345678");
    expect(form.contactEmail).toBe("ops@bsce.hu");
    expect(form.contactPhone).toBe("+36 1 234 5678");
  });

  it("folds null optional fields into empty strings", () => {
    // Regression guard: a backend that surfaces `null` for an
    // optional field must NOT crash the form's `<input bind:value>`
    // (the DOM seam expects a string).
    const partner: Partner = {
      ...SAMPLE_PARTNER,
      eu_vat_number: null,
      address_street: null,
      bank_account: null,
      contact_email: null,
      contact_phone: null,
    };
    const form = formFromPartner(partner);
    expect(form.euVatNumber).toBe("");
    expect(form.addressStreet).toBe("");
    expect(form.bankAccount).toBe("");
    expect(form.contactEmail).toBe("");
    expect(form.contactPhone).toBe("");
  });
});

describe("composePartnerInputs", () => {
  it("trims required string fields", () => {
    // Backend `validate_partner_inputs` trims display_name /
    // legal_name; the composer pre-trims so a `"  "` value surfaces
    // as the backend's actionable error rather than slipping through
    // pre-validation.
    const body = composePartnerInputs({
      ...emptyPartnerForm(),
      displayName: "  BSCE  ",
      legalName: "  BSCE Kft.  ",
      taxNumber: "  12345678-1-42  ",
    });
    expect(body.display_name).toBe("BSCE");
    expect(body.legal_name).toBe("BSCE Kft.");
    expect(body.tax_number).toBe("12345678-1-42");
  });

  it("folds empty optional fields to null", () => {
    // Backend's PartnerInputs deserialiser carries Option<String>
    // slots; empty strings here would persist as a meaningless empty
    // VARCHAR rather than NULL. The composer's empty-to-null fold
    // keeps the storage shape clean (matches the
    // `normalize_optional` posture on `aberp::partners`).
    const body = composePartnerInputs({
      ...emptyPartnerForm(),
      displayName: "X",
      legalName: "X Kft.",
      taxNumber: "12345678-1-42",
      euVatNumber: "",
      bankAccount: "   ",
      contactEmail: "",
    });
    expect(body.eu_vat_number).toBeNull();
    expect(body.bank_account).toBeNull();
    expect(body.contact_email).toBeNull();
  });

  it("emits snake_case wire field names", () => {
    // Regression guard: the backend's PartnerInputs deserialiser
    // expects snake_case keys (no `rename_all` directive on the Rust
    // struct). A drift to camelCase here would surface as a 400
    // "missing field" from the backend.
    const body = composePartnerInputs({
      ...emptyPartnerForm(),
      displayName: "X",
      legalName: "X Kft.",
      taxNumber: "12345678-1-42",
      addressStreet: "Y",
      addressPostalCode: "1011",
      addressCity: "Budapest",
    });
    expect("display_name" in body).toBe(true);
    expect("legal_name" in body).toBe(true);
    expect("tax_number" in body).toBe(true);
    expect("address_street" in body).toBe(true);
    expect("address_postal_code" in body).toBe(true);
    expect("address_city" in body).toBe(true);
    // camelCase keys must NOT leak onto the wire.
    expect("displayName" in body).toBe(false);
    expect("legalName" in body).toBe(false);
    expect("taxNumber" in body).toBe(false);
  });

  it("preserves kind verbatim", () => {
    for (const kind of ["Customer", "Supplier", "Both"] as const) {
      const body = composePartnerInputs({
        ...emptyPartnerForm(),
        displayName: "X",
        legalName: "X",
        taxNumber: "12345678-1-42",
        kind,
      });
      expect(body.kind).toBe(kind);
    }
  });
});

describe("buyerFieldsFromPartner", () => {
  it("uses legal_name on the invoice (not display_name)", () => {
    // Regulatory compliance: NAV's printed invoice carries the legal
    // name. `display_name` is the operator-friendly shorthand for the
    // list view; using it on the invoice would mismatch the tax-
    // authority's expected counterparty name.
    const fields = buyerFieldsFromPartner(SAMPLE_PARTNER);
    expect(fields.customerName).toBe(
      "Budapesti Sport-Egyesület Kft.",
    );
    expect(fields.customerTaxNumber).toBe("12345678-1-42");
  });
});

describe("filterPartners", () => {
  const rows: Partner[] = [
    { ...SAMPLE_PARTNER, id: "prt_a", display_name: "Alpha", legal_name: "Alpha Kft.", tax_number: "11111111-1-11" },
    { ...SAMPLE_PARTNER, id: "prt_b", display_name: "Bravo", legal_name: "Bravo Bt.", tax_number: "22222222-2-22" },
    { ...SAMPLE_PARTNER, id: "prt_c", display_name: "Charlie", legal_name: "Charlie Zrt.", tax_number: "33333333-3-33" },
  ];

  it("returns every row when the needle is empty", () => {
    expect(filterPartners(rows, "")).toEqual(rows);
    expect(filterPartners(rows, "   ")).toEqual(rows);
  });

  it("filters case-insensitively on display_name", () => {
    expect(filterPartners(rows, "alp")).toEqual([rows[0]]);
    expect(filterPartners(rows, "ALP")).toEqual([rows[0]]);
  });

  it("filters on legal_name too", () => {
    expect(filterPartners(rows, "Bt.")).toEqual([rows[1]]);
  });

  it("filters on tax_number", () => {
    expect(filterPartners(rows, "33333333")).toEqual([rows[2]]);
  });
});

describe("parsePartnerValidationError", () => {
  it("extracts the typed body from the Tauri-wrapped error string", () => {
    const raw =
      'backend returned 400 Bad Request for /api/partners: ' +
      '{"error":"validation_failed","fields":[' +
      '{"field":"display_name","message":"display name is required"},' +
      '{"field":"tax_number","message":"tax number must start with 8 digits"}' +
      "]}";
    const parsed = parsePartnerValidationError(raw);
    expect(parsed).not.toBeNull();
    expect(parsed!.fields.length).toBe(2);
    expect(parsed!.fields[0].field).toBe("display_name");
    expect(parsed!.fields[1].field).toBe("tax_number");
  });

  it("returns null for a malformed body", () => {
    expect(parsePartnerValidationError("network error")).toBeNull();
    expect(
      parsePartnerValidationError('backend returned 500 ISE: <html>'),
    ).toBeNull();
  });

  it("returns null when the discriminant is wrong", () => {
    // Pre-PR-54 400 bodies (e.g. plain `{error: "not found"}`) must
    // NOT misidentify as the typed shape.
    const raw =
      'backend returned 404 for /api/partners/x: {"error":"partner not found"}';
    expect(parsePartnerValidationError(raw)).toBeNull();
  });

  it("returns null when fields is not an array", () => {
    const raw =
      'backend returned 400: {"error":"validation_failed","fields":"oops"}';
    expect(parsePartnerValidationError(raw)).toBeNull();
  });
});
