// PR-72 / session-94 — vitest pins for the seller-banks helper module.
// Composer + wire-to-form mapper + client-side validator + typed-error
// parser + wizard-row group helpers are pure functions (no Svelte, no
// Tauri); pinning them in isolation surfaces regressions before the
// dev-loop renders TenantSettings or SellerConfigWizard.
//
// Mirror invariant: a backend drift that renames a field on
// `serve::SellerBankResponse` or `serve::SellerBankInputs` would
// surface here via the snake_case wire-shape assertions.

import { describe, expect, it } from "vitest";

import type { SellerBankResponse } from "./api";
import {
  composeSellerBankInputs,
  emptySellerBankForm,
  emptyWizardBankRows,
  formFromSellerBank,
  groupSellerBanksByCurrency,
  parseSellerBankValidationError,
  validateSellerBankForm,
  validateWizardBankRows,
} from "./seller-banks";

const HUF_BANK: SellerBankResponse = {
  id: "bnk_01ARZ3NDEKTSV4RRFFQ69G5FAV",
  currency: "HUF",
  account_number: "12345678-12345678-12345678",
  bank_name: "Erste Bank",
  swift_bic: "GIBAHUHB",
  is_default: true,
};

const EUR_BANK: SellerBankResponse = {
  id: "bnk_01BBBBBBBBBBBBBBBBBBBBBBBB",
  currency: "EUR",
  account_number: "HU12-3456-7890-1234-5678-9012-3456",
  bank_name: "Erste Bank",
  swift_bic: "GIBAHUHB",
  is_default: true,
};

describe("emptySellerBankForm", () => {
  it("defaults to HUF + setAsDefault=false", () => {
    const form = emptySellerBankForm();
    expect(form.currency).toBe("HUF");
    expect(form.accountNumber).toBe("");
    expect(form.bankName).toBe("");
    expect(form.swiftBic).toBe("");
    expect(form.setAsDefault).toBe(false);
  });
});

describe("formFromSellerBank", () => {
  it("maps the wire shape to the camelCase form one-to-one", () => {
    const form = formFromSellerBank(HUF_BANK);
    expect(form.currency).toBe("HUF");
    expect(form.accountNumber).toBe(HUF_BANK.account_number);
    expect(form.bankName).toBe(HUF_BANK.bank_name);
    expect(form.swiftBic).toBe(HUF_BANK.swift_bic);
    // setAsDefault is intentionally false on Edit; the modal hides
    // the checkbox when the row is already the default (per brief).
    expect(form.setAsDefault).toBe(false);
  });
});

describe("composeSellerBankInputs", () => {
  it("trims string fields and snake_cases the wire body", () => {
    const body = composeSellerBankInputs({
      currency: "EUR",
      accountNumber: "  HU12-3456  ",
      bankName: "  Erste Bank  ",
      swiftBic: "  gibahuhb  ",
      setAsDefault: true,
    });
    expect(body.currency).toBe("EUR");
    expect(body.account_number).toBe("HU12-3456");
    expect(body.bank_name).toBe("Erste Bank");
    // SWIFT/BIC uppercased on the way to the wire — the operator may
    // paste a lowercase BIC; the PR-71 SWIFT-country inference reads
    // positions 4-5 of the canonical upper-case form.
    expect(body.swift_bic).toBe("GIBAHUHB");
    expect(body.set_as_default).toBe(true);
  });
});

describe("validateSellerBankForm", () => {
  it("accepts a fully-populated form", () => {
    const form = formFromSellerBank(HUF_BANK);
    const v = validateSellerBankForm(form);
    expect(v.ok).toBe(true);
    expect(v.accountNumber).toBeNull();
    expect(v.bankName).toBeNull();
    expect(v.swiftBic).toBeNull();
  });

  it("surfaces every missing field at once (A157 pattern)", () => {
    const v = validateSellerBankForm(emptySellerBankForm());
    expect(v.ok).toBe(false);
    expect(v.accountNumber).not.toBeNull();
    expect(v.bankName).not.toBeNull();
    expect(v.swiftBic).not.toBeNull();
  });
});

describe("parseSellerBankValidationError", () => {
  it("extracts the typed body from the Tauri-wrapped error string", () => {
    const wrapped =
      'backend returned 400 Bad Request for /api/seller/banks: ' +
      '{"error":"validation_failed","fields":[' +
      '{"field":"currency","message":"Pénznem nem támogatott: USD\\nUnsupported currency USD."},' +
      '{"field":"accountNumber","message":"Bankszámlaszám kötelező.\\nAccount number is required."}' +
      "]}";
    const body = parseSellerBankValidationError(wrapped);
    expect(body).not.toBeNull();
    expect(body!.error).toBe("validation_failed");
    expect(body!.fields).toHaveLength(2);
    expect(body!.fields[0].field).toBe("currency");
    expect(body!.fields[0].message).toContain("Pénznem");
    expect(body!.fields[0].message).toContain("Unsupported");
    expect(body!.fields[1].field).toBe("accountNumber");
  });

  it("returns null for an unrelated error string", () => {
    expect(parseSellerBankValidationError("backend returned 500")).toBeNull();
    expect(parseSellerBankValidationError("{not-json")).toBeNull();
  });
});

describe("groupSellerBanksByCurrency", () => {
  it("groups by currency in declaration order", () => {
    const groups = groupSellerBanksByCurrency([HUF_BANK, EUR_BANK]);
    expect(groups).toHaveLength(2);
    expect(groups[0].currency).toBe("HUF");
    expect(groups[0].banks).toHaveLength(1);
    expect(groups[1].currency).toBe("EUR");
  });

  it("preserves first-appearance order across mixed input", () => {
    const groups = groupSellerBanksByCurrency([EUR_BANK, HUF_BANK]);
    expect(groups[0].currency).toBe("EUR");
    expect(groups[1].currency).toBe("HUF");
  });

  it("returns an empty array for empty input", () => {
    expect(groupSellerBanksByCurrency([])).toEqual([]);
  });
});

describe("emptyWizardBankRows + validateWizardBankRows", () => {
  it("seeds with one HUF default row", () => {
    const rows = emptyWizardBankRows();
    expect(rows).toHaveLength(1);
    expect(rows[0].currency).toBe("HUF");
    expect(rows[0].setAsDefault).toBe(true);
    expect(rows[0].rowKey).toBeTruthy();
  });

  it("rejects an empty bank-row list at submit", () => {
    const v = validateWizardBankRows([]);
    expect(v.ok).toBe(false);
    if (!v.ok) {
      expect(v.summary).toMatch(/at least one bank/i);
    }
  });

  it("rejects rows with missing fields (per-row inline errors)", () => {
    const v = validateWizardBankRows(emptyWizardBankRows());
    expect(v.ok).toBe(false);
    if (!v.ok) {
      const row1Errors = v.rowErrors.get("row-1");
      expect(row1Errors).toBeDefined();
      expect(row1Errors!.accountNumber).not.toBeNull();
    }
  });

  it("rejects a currency with zero defaults at submit", () => {
    const v = validateWizardBankRows([
      {
        currency: "HUF",
        accountNumber: "HUF-A",
        bankName: "Bank One",
        swiftBic: "GIBAHUHB",
        setAsDefault: false,
        rowKey: "row-1",
      },
    ]);
    expect(v.ok).toBe(false);
    if (!v.ok) {
      expect(v.summary).toMatch(/HUF/);
    }
  });

  it("rejects a currency with two defaults at submit", () => {
    const v = validateWizardBankRows([
      {
        currency: "HUF",
        accountNumber: "HUF-A",
        bankName: "Bank A",
        swiftBic: "GIBAHUHB",
        setAsDefault: true,
        rowKey: "row-1",
      },
      {
        currency: "HUF",
        accountNumber: "HUF-B",
        bankName: "Bank B",
        swiftBic: "GIBAHUHB",
        setAsDefault: true,
        rowKey: "row-2",
      },
    ]);
    expect(v.ok).toBe(false);
    if (!v.ok) {
      expect(v.summary).toMatch(/HUF/);
      expect(v.summary).toMatch(/currently 2/);
    }
  });

  it("accepts mixed-currency rows with one default each", () => {
    const v = validateWizardBankRows([
      {
        currency: "HUF",
        accountNumber: "HUF-A",
        bankName: "Bank A",
        swiftBic: "GIBAHUHB",
        setAsDefault: true,
        rowKey: "row-1",
      },
      {
        currency: "EUR",
        accountNumber: "EUR-A",
        bankName: "Bank A EUR",
        swiftBic: "GIBAHUHB",
        setAsDefault: true,
        rowKey: "row-2",
      },
    ]);
    expect(v.ok).toBe(true);
    if (v.ok) {
      expect(v.rows).toHaveLength(2);
    }
  });
});
