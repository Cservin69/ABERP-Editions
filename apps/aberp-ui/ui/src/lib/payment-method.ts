// S160 / ADR-0050 — pure helper for the IssueInvoice form's "Fizetési
// mód / Payment method" dropdown. No Svelte runes, no DOM — so the
// option list + labels are pinnable under vitest without mounting the
// component (A156 / A161 mirror-invariant precedent).
//
// The closed vocab mirrors the Rust `aberp_billing::PaymentMethod` enum
// (SCREAMING_SNAKE NAV tokens) and the TS `InvoicePaymentMethod` union
// in `api.ts`. NAV's `paymentMethodType` is closed with NO free-text
// companion (unlike `unitOfMeasure`/`unitOfMeasureOwn`) — so "OTHER"
// (Egyéb) is the catch-all and there is no "reveal free-text" affordance.
// A drift between this table and the Rust enum surfaces at the Rust
// `labels_pinned` / `nav_token_round_trips` tests and this module's
// vitest.

import type { InvoicePaymentMethod } from "./api";

/** Bilingual label metadata for one payment-method option. Hungarian is
 * primary (rendered on the printed invoice); English is the parenthesised
 * secondary in the bilingual dropdown. */
export interface PaymentMethodOption {
  /** Wire value — the bare NAV token sent on `IssueInvoiceRequest.paymentMethod`. */
  value: InvoicePaymentMethod;
  /** Hungarian label (primary). */
  labelHu: string;
  /** English label (secondary). */
  labelEn: string;
}

/** The closed-vocab option list for the dropdown, in operator-facing
 * order (Átutalás first — it is the default and dominant case). Labels
 * mirror `PaymentMethod::hu_label` / `en_label` on the Rust side. */
export function paymentMethodOptions(): PaymentMethodOption[] {
  return [
    { value: "TRANSFER", labelHu: "Átutalás", labelEn: "Bank transfer" },
    { value: "CASH", labelHu: "Készpénz", labelEn: "Cash" },
    { value: "CARD", labelHu: "Bankkártya", labelEn: "Card" },
    { value: "VOUCHER", labelHu: "Utalvány", labelEn: "Voucher" },
    { value: "OTHER", labelHu: "Egyéb", labelEn: "Other" },
  ];
}

/** The default payment method for a fresh invoice — Átutalás (bank
 * transfer), matching the pre-S160 hardcoded behaviour and the Rust
 * `PaymentMethod::default()`. */
export const DEFAULT_PAYMENT_METHOD: InvoicePaymentMethod = "TRANSFER";

/** Bilingual `"Hu (En)"` label for a payment method — used by the
 * dropdown option text. Falls back to the raw token for an unknown value
 * (defensive; the closed union should make this unreachable). */
export function paymentMethodLabel(value: InvoicePaymentMethod): string {
  const opt = paymentMethodOptions().find((o) => o.value === value);
  return opt ? `${opt.labelHu} (${opt.labelEn})` : value;
}
