// PR-51 / session-71 — vitest pins on the SellerConfigWizard's
// composer + validator + typed-error parser. The composer is the
// load-bearing contract between the wizard form's camelCase state
// and the backend route's snake_case + nested wire shape; a drift
// here breaks the wizard silently (the backend would 400 on every
// submit). The validator + parser are SPA-only operator UX, but
// they're tested too because the parser's substring matching is
// brittle to backend forwarder-string drift.

import { describe, expect, it } from "vitest";

import {
  composeSellerConfigBody,
  DEFAULT_SELLER_CONFIG_FORM,
  parseSetupSellerInfoErrorBody,
  validateSellerConfig,
  type SellerConfigForm,
} from "./seller-config";

function goodForm(): SellerConfigForm {
  return {
    legalName: "Áben Consulting KFT.",
    taxNumber: "24904362-2-41",
    euVatNumber: "HU24904362",
    addressCountryCode: "HU",
    addressPostalCode: "1037",
    addressCity: "Budapest",
    addressStreet: "Visszatérő köz 6",
    bankAccountNumber: "12345678-12345678-12345678",
    iban: "LT14 3250 0448 1318 6860",
    bankName: "Revolut",
    swiftBic: "REVOLT21",
  };
}

describe("composeSellerConfigBody", () => {
  it("maps every camelCase field to the snake_case + nested wire shape", () => {
    const body = composeSellerConfigBody(goodForm());
    expect(body).toEqual({
      legal_name: "Áben Consulting KFT.",
      tax_number: "24904362-2-41",
      eu_vat_number: "HU24904362",
      address: {
        country_code: "HU",
        postal_code: "1037",
        city: "Budapest",
        street: "Visszatérő köz 6",
      },
      bank: {
        account_number: "12345678-12345678-12345678",
        iban: "LT14 3250 0448 1318 6860",
        name: "Revolut",
        swift_bic: "REVOLT21",
      },
    });
  });

  it("trims required fields", () => {
    const form = goodForm();
    form.legalName = "  Áben Consulting KFT.  ";
    form.taxNumber = " 24904362-2-41 ";
    form.addressCity = "  Budapest  ";
    const body = composeSellerConfigBody(form);
    expect(body.legal_name).toBe("Áben Consulting KFT.");
    expect(body.tax_number).toBe("24904362-2-41");
    expect(body.address.city).toBe("Budapest");
  });

  // Optional fields with empty-string values must fold to `null` so
  // the backend's `Option<String>` deserialiser does the right thing.
  // A regression that sent `""` would cause the backend to write
  // `eu_vat_number = ""` lines to seller.toml, polluting the file.
  it("folds blank optional fields to null", () => {
    const form = goodForm();
    form.euVatNumber = "";
    form.bankAccountNumber = "  ";
    form.iban = "";
    form.bankName = "";
    form.swiftBic = "";
    const body = composeSellerConfigBody(form);
    expect(body.eu_vat_number).toBeNull();
    expect(body.bank.account_number).toBeNull();
    expect(body.bank.iban).toBeNull();
    expect(body.bank.name).toBeNull();
    expect(body.bank.swift_bic).toBeNull();
  });
});

describe("validateSellerConfig", () => {
  it("accepts the default-good form", () => {
    const v = validateSellerConfig(goodForm());
    expect(v.ok).toBe(true);
    expect(v.legalName).toBeNull();
    expect(v.taxNumber).toBeNull();
    expect(v.addressCountryCode).toBeNull();
    expect(v.addressPostalCode).toBeNull();
    expect(v.addressCity).toBeNull();
    expect(v.addressStreet).toBeNull();
  });

  it("rejects blank legal name", () => {
    const form = goodForm();
    form.legalName = "   ";
    const v = validateSellerConfig(form);
    expect(v.ok).toBe(false);
    expect(v.legalName).toMatch(/legal name/i);
  });

  it("rejects blank tax number", () => {
    const form = goodForm();
    form.taxNumber = "";
    const v = validateSellerConfig(form);
    expect(v.ok).toBe(false);
    expect(v.taxNumber).toMatch(/ADÓSZÁM|tax number/i);
  });

  // Client-side shape gate matches the Rust-side
  // parse_hungarian_tax_number: bare 8-digit must fail.
  it("rejects malformed tax number `24904362` (missing dashes)", () => {
    const form = goodForm();
    form.taxNumber = "24904362";
    const v = validateSellerConfig(form);
    expect(v.ok).toBe(false);
    expect(v.taxNumber).toMatch(/xxxxxxxx-y-zz|ADÓSZÁM/);
  });

  it("rejects malformed tax number `24904362-24-41` (wrong vatCode width)", () => {
    const form = goodForm();
    form.taxNumber = "24904362-24-41";
    const v = validateSellerConfig(form);
    expect(v.ok).toBe(false);
    expect(v.taxNumber).not.toBeNull();
  });

  it("rejects blank required address fields independently", () => {
    const form = goodForm();
    form.addressCity = "";
    form.addressStreet = "  ";
    const v = validateSellerConfig(form);
    expect(v.ok).toBe(false);
    expect(v.addressCity).not.toBeNull();
    expect(v.addressStreet).not.toBeNull();
    // Other required fields stayed valid.
    expect(v.legalName).toBeNull();
    expect(v.taxNumber).toBeNull();
  });

  it("default form is INVALID (blank required identity)", () => {
    const v = validateSellerConfig({ ...DEFAULT_SELLER_CONFIG_FORM });
    expect(v.ok).toBe(false);
    // Country code defaults to "HU", so that field passes.
    expect(v.addressCountryCode).toBeNull();
  });
});

describe("parseSetupSellerInfoErrorBody", () => {
  it("parses the typed 400 body out of the Tauri-forwarder string", () => {
    const forwarded =
      'backend returned 400 Bad Request for /api/setup-seller-info: {"error":"validation_failed","fields":[{"field":"legalName","message":"Legal name is required"},{"field":"taxNumber","message":"supplier tax number `24904362` is not a valid Hungarian ADÓSZÁM (expected three dash-separated segments; expected `xxxxxxxx-y-zz`, e.g. `24904362-2-41`)"}]}';
    const parsed = parseSetupSellerInfoErrorBody(forwarded);
    expect(parsed).not.toBeNull();
    expect(parsed!.error).toBe("validation_failed");
    expect(parsed!.fields).toHaveLength(2);
    expect(parsed!.fields[0]).toEqual({
      field: "legalName",
      message: "Legal name is required",
    });
    expect(parsed!.fields[1].field).toBe("taxNumber");
    expect(parsed!.fields[1].message).toContain("ADÓSZÁM");
  });

  it("returns null on a non-validation error string", () => {
    const forwarded =
      "backend returned 500 Internal Server Error for /api/setup-seller-info: {\"error\":\"internal error\"}";
    const parsed = parseSetupSellerInfoErrorBody(forwarded);
    expect(parsed).toBeNull();
  });

  it("returns null when the body is not JSON at all", () => {
    expect(parseSetupSellerInfoErrorBody("network error")).toBeNull();
  });

  it("returns null when fields array entries are malformed", () => {
    const forwarded =
      'backend returned 400 Bad Request for /api/setup-seller-info: {"error":"validation_failed","fields":["just a string"]}';
    expect(parseSetupSellerInfoErrorBody(forwarded)).toBeNull();
  });
});
