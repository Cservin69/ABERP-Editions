// S160 / ADR-0050 — vitest pin for the payment-method dropdown helper.
//
// Mirror invariant: this option list + label table must agree with the
// Rust `aberp_billing::PaymentMethod` enum (the `nav_token` /
// `hu_label` / `en_label` pins) and the TS `InvoicePaymentMethod` union
// in `api.ts`. A drift (renamed token, dropped variant, swapped label)
// surfaces here before it reaches a NAV submission.

import { describe, expect, it } from "vitest";

import {
  DEFAULT_PAYMENT_METHOD,
  paymentMethodLabel,
  paymentMethodOptions,
} from "./payment-method";

describe("paymentMethodOptions", () => {
  it("returns the full closed NAV vocab with bilingual labels", () => {
    expect(paymentMethodOptions()).toEqual([
      { value: "TRANSFER", labelHu: "Átutalás", labelEn: "Bank transfer" },
      { value: "CASH", labelHu: "Készpénz", labelEn: "Cash" },
      { value: "CARD", labelHu: "Bankkártya", labelEn: "Card" },
      { value: "VOUCHER", labelHu: "Utalvány", labelEn: "Voucher" },
      { value: "OTHER", labelHu: "Egyéb", labelEn: "Other" },
    ]);
  });

  it("defaults to TRANSFER (Átutalás), matching the pre-S160 hardcoded emit", () => {
    expect(DEFAULT_PAYMENT_METHOD).toBe("TRANSFER");
    // The default is a real option in the list.
    expect(
      paymentMethodOptions().some((o) => o.value === DEFAULT_PAYMENT_METHOD),
    ).toBe(true);
  });

  it("renders a bilingual 'Hu (En)' label for each variant", () => {
    expect(paymentMethodLabel("CASH")).toBe("Készpénz (Cash)");
    expect(paymentMethodLabel("OTHER")).toBe("Egyéb (Other)");
  });
});
