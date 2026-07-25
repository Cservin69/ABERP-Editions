// PR-44ζ / session-59 — form-state + form-to-request-body composer
// for the SPA's IssueInvoice form. Kept in a pure module (no Svelte
// runes; no DOM) so the composer is testable under vitest without
// mounting a component.
//
// The composer is the load-bearing seam between the operator-typed
// form values and the wire shape `serve::IssueInvoiceRequest`
// expects: the backend Deserializer is strict (uppercase currency,
// camelCase JSON field names), and a regression that mis-mints any
// of those would surface as a 400 rather than a silent issuance with
// wrong data.
//
// Pinned by `issue-invoice.test.ts` per the A156 / A161 mirror-
// invariant precedent.

import type {
  Currency,
  CustomerVatStatusBody,
  InvoicePaymentMethod,
  IssueInvoiceRequest,
  Product,
  ProductUnit,
  SellerBankResponse,
  VatRateKindBody,
} from "./api";
import { DEFAULT_PAYMENT_METHOD } from "./payment-method";
import {
  formatMinorToInput,
  parseAmountToMinor,
  parseDecimalQuantity,
} from "./format";
import {
  addDays,
  comfortZone,
  DEFAULT_PAYMENT_OFFSET_DAYS,
  overrideKindForZone,
  todayLocalIso,
  type DeliveryDateOverride,
  type IsoDate,
} from "./invoice-dates";

/** PR-88 / session-113 — per-line form state.
 *
 * `unitPriceInput` is the OPERATOR-TYPED RAW STRING from the form's
 * unit-price input. The composer converts it to integer minor units
 * via [`parseAmountToMinor`] at submit time, using the form's
 * currency to drive the major→minor scaling (`340` typed under EUR
 * becomes 34000 cents; under HUF, 340 forints).
 *
 * Pre-PR-88 this field was `unitPriceMinor: number` bound directly
 * to a `type="number"` input — the operator's typed digits were
 * persisted verbatim as MINOR units. That worked for HUF (HUF is
 * 0-decimal so major == minor) but produced a 100× underbill for
 * EUR: `340` typed → 340 cents on the wire = 3.40 EUR instead of
 * 340.00 EUR. Ervin issued one wrong-amount invoice in live test
 * before catching it. The fix is to read raw operator input as a
 * string and convert at compose time so the major-unit
 * interpretation is canonical.
 *
 * `vatRatePercent` remains integer because its `<input type="number">`
 * is unambiguous (no decimal-separator ambiguity, no minor-unit scaling).
 *
 * S157 — `quantity` is now `quantityInput: string` for the same reason
 * `unitPriceInput` is a string: the operator may type a decimal with
 * either separator (`1.5` or `1,5`), so we capture the raw string and
 * parse it at compose time via `parseDecimalQuantity`. */
export interface LineFormState {
  description: string;
  quantityInput: string;
  unitPriceInput: string;
  vatRatePercent: number;
  /** ADR-0101 — per-line VAT rate-kind selector value. Defaults to
   * `"Percent"` (the numeric path — backward-compat). For any non-`Percent`
   * kind the UI LOCKS `vatRatePercent` to 0 and disables the VAT% input
   * (exempt / reverse-charge line VAT is always 0); the composer also forces
   * `vatRatePercent: 0` on the wire. Set exclusively via [`applyVatKind`] so
   * the rate-lock invariant lives in one place. */
  vatRateKind: VatRateKindBody;
  /** PR-82 — operator-typed per-line note ("Megjegyzés"). Empty
   * string when blank; `composeIssueInvoiceBody` normalises to
   * `null` on the wire so the backend sees a clean "no note"
   * signal. Recipient-facing only — NEVER reaches the NAV XML. */
  note: string;
  /** PR-100 / S406 — UI-only state that records the default currency
   * of the product the operator most-recently picked for this line.
   * Set by `applyProductPick()`; `null` for one-off lines (operator
   * typed a free-text description, no product picked). The composer
   * does NOT read this field — it never reaches the wire body.
   *
   * S406 — this is now a PERSISTENT stamp of the picked product's
   * currency, NOT a transient mismatch snapshot. Picking a product
   * auto-flips the invoice currency to the product's default
   * ([`applyProductPick`]) so there is no mismatch at pick time; the
   * stamp only earns a warning later, if the operator overrides the
   * invoice currency away from it. The per-line warning is therefore
   * DERIVED ([`lineCurrencyMismatchWarning`]) from this stamp vs the
   * current invoice currency — code-derived, never an operator-cleared
   * snapshot. Travelling on the line (rather than a sibling
   * `Record<number, …>` on the component) keeps the warning correct
   * across add/remove-line shuffles. */
  productCurrency?: Currency | null;
  /** S159 — the unit of measure stamped by `pickProduct()` from the
   * picked product's `unit`. `null`/absent for one-off freetext lines
   * (operator typed a description without picking a product); the
   * composer emits it as `lines[i].unit` and the backend's NAV emit
   * falls back to `<unitOfMeasure>PIECE</...>` for a null unit. Cleared
   * to `null` only implicitly — re-picking a product overwrites it. */
  unit?: ProductUnit | null;
}

/** PR-44ζ — top-level form state. Captures every operator-typed
 * value the form exposes; the composer reshapes it into the wire
 * `IssueInvoiceRequest`.
 *
 * PR-53 / session-73 — supplier fields removed from the form (the
 * backend now reads seller identity from the per-tenant
 * `seller.toml` populated via the wizard). Operator-typed values are
 * customer + currency + line items only. */
export interface IssueInvoiceFormState {
  /** PR-97 / ADR-0048 — closed-vocab buyer-kind discriminator. Bound
   * to the form's three-option radio. Drives whether `customerTaxNumber`
   * is required + editable (`Domestic`) or disabled + ignored
   * (`PrivatePerson` — input disabled but visible; `Other` — v1
   * named-deferred per ADR-0048 §7, the SPA shows the radio option
   * disabled with a v2 hint). Populated from the operator-selected
   * partner via `buyerFieldsFromPartner`. */
  customerVatStatus: CustomerVatStatusBody;
  /** PR-97 / ADR-0048 (Ervin override 1) — saved-partner id when the
   * operator picked a buyer via the typeahead; `null` for one-off
   * buyers (typed name without selecting). Composer emits this on
   * the wire body so the backend can increment the partner's
   * counter for the field-selective lock. */
  customerPartnerId: string | null;
  customerTaxNumber: string;
  customerName: string;
  /** PR-77 / session-101 — customer-address quartet. Populated from
   * the operator-selected partner via `buyerFieldsFromPartner`
   * (PR-54 buyer combobox). Required for any Hungarian-business
   * buyer; preflight rejects an invoice whose customer.address is
   * absent or any of these four fields is blank-after-trim. The
   * `customerCountryCode` is locked to `HU` for every Hungarian-
   * DOMESTIC buyer today; widening to non-Hungarian buyers is named-
   * deferred per the PR-77 handoff. */
  customerCountryCode: string;
  customerPostalCode: string;
  customerCity: string;
  customerStreet: string;
  /** ADR-0102 — the buyer's EU community VAT number. Shown + required
   * ONLY when `customerVatStatus === "Other"` (a foreign-EU business
   * buyer); empty for Domestic/PrivatePerson. Populated from the picked
   * partner's `eu_vat_number` via `buyerFieldsFromPartner`; the composer
   * emits it on the wire body (snapshotted onto the issued invoice) only
   * for Other. Structurally validated (VIES shape) at the backend
   * preflight + partner-form gate. */
  customerCommunityVatNumber: string;
  currency: Currency;
  lines: LineFormState[];
  /** PR-73 / ADR-0040 §addendum — operator-selected bank account id
   * (the `bnk_<26-char>` value from `listSellerBanks`). `null`
   * means "use the per-currency default" — the SPA's bank picker
   * defaults this to the entry with `is_default: true` for the
   * current `currency` but lets the operator switch. The composer
   * emits `null` as `bankAccountId: null` on the wire; the backend
   * resolver treats `null` the same as missing-field and falls back
   * to the per-currency default. */
  bankAccountId: string | null;
  /** PR-82 — operator-typed per-invoice global note ("Megjegyzés").
   * Empty string when the textarea is blank; the composer
   * normalises to `null` on the wire so the backend sees a clean
   * "no note" signal. Recipient-facing only — NEVER reaches the
   * NAV InvoiceData XML. */
  invoiceNote: string;
  /** PR-84 — operator-visible invoice date (Számla kelte). Read-only
   * in the UI; defaulted to today's local date. The server stamps the
   * TRUE issue date at issuance time (immutable, never trusts the
   * client clock); this is purely a display default for the form's
   * date section + an anchor for the payment-deadline and delivery-
   * date pickers. Canonical YYYY-MM-DD. */
  invoiceDate: IsoDate;
  /** PR-84 — operator-supplied payment deadline (Fizetési határidő).
   * Bidirectional: the form exposes both an offset-days input and an
   * absolute date picker; the two update each other live. This field
   * carries the resolved absolute date (the offset is derived on
   * render via `daysBetween(invoiceDate, paymentDeadline)`). */
  paymentDeadline: IsoDate;
  /** PR-84 — operator-chosen delivery / fulfillment date (Teljesítési
   * dátum). REGULATORY: drives the NAV `<invoiceDeliveryDate>` field.
   * Defaults to invoiceDate; the operator can pick any date but
   * out-of-range choices (before invoiceDate OR after paymentDeadline)
   * trigger an inline "Are you sure?" confirm that captures the audit
   * override. */
  deliveryDate: IsoDate;
  /** PR-84 — comfort-zone audit discriminant the operator has
   * confirmed for the current `deliveryDate`. `null` when the
   * delivery date is in range (default, no audit flag); a non-null
   * value persists across edits until the operator picks a different
   * date OR confirms the new override. The composer stamps the
   * current value verbatim into the wire body's `deliveryDateOverride`
   * field. */
  deliveryDateOverride: DeliveryDateOverride;
  /** PR-92 / ADR-0047 — default-on "Email to buyer" toggle. `true`
   * means the post-issue auto-send fires; `false` means the operator
   * has opted this invoice out of the auto-send (the manual send
   * button on InvoiceDetail still works). Seeded to `true` in
   * `emptyForm` so silence-by-omission can never suppress a send. */
  emailBuyerOnIssue: boolean;
  /** PR-99 Item 4 Part B — default-on "Submit to NAV on issue" toggle.
   * Mirrors the email toggle's posture: `true` lets the backend fire
   * the same `/api/invoices/:id/submit` path immediately after the
   * issue tx commits (and then poll for the terminal ack), so the
   * operator does not have to navigate to InvoiceDetail and click
   * Submit a second time. `false` leaves the invoice in `Ready` so
   * the operator can submit manually later (the typical use case is
   * a draft the operator wants to review more before NAV sees it). */
  submitToNavOnIssue: boolean;
  /** S160 / ADR-0050 — operator-selected payment method (Fizetési mód).
   * Bound to a dropdown next to the dates/currency block; seeded to
   * `TRANSFER` (Átutalás) in `emptyForm` so the default matches the
   * pre-S160 hardcoded emit. The composer sends it verbatim as
   * `paymentMethod`; the backend snapshots it on the invoice and emits
   * `<paymentMethod>{token}</...>`. */
  paymentMethod: InvoicePaymentMethod;
  /** PR-203 / S203 — operator-typed per-invoice email recipient override
   * ("Email-címzett(ek)"). Comma-separated address list; empty string
   * means "no override, fall back to partner.email". When a saved
   * partner is picked via the typeahead, `pickPartner` pre-fills this
   * from `partner.contact_email`; the operator can edit it for THIS
   * invoice only — editing does NOT write back to the partner master.
   * One-off / inline buyers (no partner picked) start with an empty
   * field; operator types directly to enable the send. */
  emailRecipientOverride: string;
}

/** PR-44ζ — sensible defaults for an empty form. The 27% VAT rate is
 * the Hungarian standard rate; HUF is the default currency (matches
 * the CLI's default). One empty line is included so the form is
 * editable on first paint without a separate "+ Add line" click. */
export function emptyForm(): IssueInvoiceFormState {
  const today = todayLocalIso();
  // PR-84 — payment deadline seeds to `today + DEFAULT_PAYMENT_OFFSET_DAYS`
  // (a sensible business default per the brief; 8 days). The form's
  // bidirectional control lets the operator edit either side of the
  // pair. The unwrap is safe — `todayLocalIso()` produces a well-formed
  // YYYY-MM-DD and `addDays` only returns null on malformed input.
  const defaultDeadline = addDays(today, DEFAULT_PAYMENT_OFFSET_DAYS) ?? today;
  return {
    // PR-97 / ADR-0048 — defaults to Domestic for fresh-form open.
    // pickPartner overwrites this from the partner's stored field.
    customerVatStatus: "Domestic",
    // PR-97 / ADR-0048 (Ervin override 1) — no saved partner picked
    // yet on form-open; `pickPartner` stamps the id when the operator
    // selects from the typeahead.
    customerPartnerId: null,
    customerTaxNumber: "",
    customerName: "",
    // PR-77 / session-101 — customer address fields seed to empty
    // strings; the operator-selected partner populates them via
    // `buyerFieldsFromPartner`. A required-by-NAV submission with any
    // of these blank trips the preflight gate.
    customerCountryCode: "",
    customerPostalCode: "",
    customerCity: "",
    customerStreet: "",
    // ADR-0102 — empty until an Other (EU) partner is picked / the
    // operator types an EU VAT number.
    customerCommunityVatNumber: "",
    currency: "HUF",
    lines: [emptyLine()],
    // PR-73 — `null` means "use the per-currency default"; the
    // IssueInvoice.svelte effect re-runs whenever `currency` changes
    // and pre-populates this from the currency's `is_default` entry.
    bankAccountId: null,
    // PR-82 — invoice-level note seeds blank; operator opt-in.
    invoiceNote: "",
    // PR-84 — three invoice-date fields seeded for the form's first
    // paint. The display value mirrors today; the server stamps the
    // real issue date at issuance time. Delivery date defaults to the
    // invoice date (the common case — supply delivered same day as
    // invoicing); operator can pick any date with the comfort-zone
    // confirm UX.
    invoiceDate: today,
    paymentDeadline: defaultDeadline,
    deliveryDate: today,
    deliveryDateOverride: null,
    // PR-92 / ADR-0047 — default-on. Silence-by-omission would be
    // the wrong default for a buyer-comms product (the whole point
    // of the app is the buyer receiving the invoice).
    emailBuyerOnIssue: true,
    // PR-99 Item 4 Part B — default-on. The dominant operator path
    // is "issue + submit + see SAVED" inside the same minute; opting
    // out is the rare case (drafting before NAV sees it).
    submitToNavOnIssue: true,
    // S160 / ADR-0050 — default to Átutalás (bank transfer), matching
    // the pre-S160 hardcoded behaviour. The operator overrides per
    // invoice (primarily Készpénz for rare cash payments).
    paymentMethod: DEFAULT_PAYMENT_METHOD,
    // PR-203 / S203 — recipient override seeds empty. A picked partner's
    // `pickPartner` callback overwrites this from `partner.contact_email`;
    // a one-off / inline buyer keeps it empty until the operator types.
    emailRecipientOverride: "",
  };
}

/** PR-44ζ — sensible defaults for a freshly-added line.
 *
 * PR-88 / session-113 — `unitPriceInput` seeds to an empty string;
 * the form's required-attribute on the text input forces the
 * operator to type a value before submission. A 0-default would
 * silently round-trip through the parser as `null` (rejected via
 * the empty-string arm) which the backend preflight then catches
 * as `LineItemUnitPriceNonPositive` — but presenting an empty
 * input matches the "blank canvas" UX a fresh line should have. */
export function emptyLine(): LineFormState {
  return {
    description: "",
    // S157 — quantity seeds to "1" (the common case); the operator can
    // overwrite with any positive decimal (`1.5`, `0.25`).
    quantityInput: "1",
    unitPriceInput: "",
    vatRatePercent: 27,
    // ADR-0101 — default to the numeric `Percent` path (27% is the
    // Hungarian standard rate, set above). Backward-compat: an untouched
    // line composes exactly as pre-0101.
    vatRateKind: "Percent",
    // PR-82 — per-line note seeds blank; operator opt-in.
    note: "",
    // PR-100 / S406 — no product picked yet on a fresh line.
    productCurrency: null,
    // S159 — no unit until a product is picked; null → PIECE fallback.
    unit: null,
  };
}

/** S406 — apply a saved-product pick to one line of the form, and
 * AUTO-FLIP the invoice currency to the product's default currency.
 *
 * Ervin's brief (2026-06-14): "When choosing the product it should
 * flip currency and Default bank account but would still remain
 * overwritable." Picking a EUR product while the invoice is HUF
 * flips the invoice to EUR (and the bank picker re-defaults to the
 * EUR bank via [`resolveBankForCurrency`] — driven by the component's
 * currency-change effect, NOT here, so the bank-defaulting rule lives
 * in exactly one place). Fewer clicks: the operator no longer has to
 * change the currency by hand after picking a product whose price is
 * quoted in another currency.
 *
 * Pure — returns a fresh form; no side effects. The unit-price input
 * is formatted in the PRODUCT's currency (which, post-flip, equals the
 * invoice currency) so the displayed value matches the product's
 * saved price. `productCurrency` is stamped on the line so a later
 * operator override of the invoice currency surfaces the derived
 * mismatch warning ([`lineCurrencyMismatchWarning`]).
 *
 * The flip is a SUGGESTION: both the currency dropdown and the bank
 * picker remain operator-overwritable after the pick. */
export function applyProductPick(
  form: IssueInvoiceFormState,
  lineIndex: number,
  product: Product,
): IssueInvoiceFormState {
  const lines = form.lines.map((line, i) => {
    if (i !== lineIndex) return line;
    return {
      ...line,
      description: product.name,
      // Format using the product's currency — post-flip this equals
      // the invoice currency, so the operator sees the saved price
      // verbatim (no silent conversion).
      unitPriceInput: formatMinorToInput(
        product.unit_price_minor,
        product.currency,
      ),
      // S406 — persistent stamp of the picked product's currency (was
      // PR-100's transient mismatch snapshot). Drives the derived
      // warning if the operator later overrides the invoice currency.
      productCurrency: product.currency,
      // S159 — stamp the unit of measure for the NAV `<unitOfMeasure>` emit.
      unit: product.unit,
    };
  });
  // S406 — auto-flip the invoice currency to the product's default.
  // Idempotent when they already match.
  return { ...form, currency: product.currency, lines };
}

/** ADR-0101 — one operator-selectable VAT rate-kind option: its wire
 * value + a bilingual human label + a short bilingual explanation the SPA
 * shows beneath the selector so the operator knows what each category means
 * (HÜLYE-BIZTOS: never make the operator guess what "EUFAD37" is). */
export interface VatKindOption {
  value: VatRateKindBody;
  labelHu: string;
  labelEn: string;
  /** Short "what this means" line — statutory ref + plain wording. */
  hintHu: string;
  hintEn: string;
}

/** ADR-0101 — the five operator-selectable VAT rate-kinds, in menu order.
 * `Percent` first (the default / dominant path); then the four
 * Ervin-confirmed non-`Percent` kinds. The named-deferred backend kinds are
 * intentionally ABSENT — they are not issuable, so listing them would be a
 * dead option (the operator would pick one and hit a preflight rejection).
 *
 * This is the SINGLE source of truth for the selector's options + labels;
 * the Svelte component renders from it and the vitest pin asserts its
 * membership equals the `VatRateKindBody` union. */
export const VAT_KIND_OPTIONS: readonly VatKindOption[] = [
  {
    value: "Percent",
    labelHu: "Százalékos ÁFA (27/18/5/0%)",
    labelEn: "Percentage VAT (27/18/5/0%)",
    hintHu: "Normál, számszerű ÁFA-kulcs. A magyar belföldi értékesítés alapesete.",
    hintEn: "Standard numeric VAT rate. The default for domestic sales.",
  },
  {
    value: "AamExempt",
    labelHu: "Alanyi adómentes (AAM)",
    labelEn: "Subjective exemption (AAM)",
    hintHu: "Alanyi adómentesség [Áfa tv. 187–196. §]. 0% ÁFA, nincs felszámított adó.",
    hintEn: "Small-taxpayer exemption. 0% VAT, no tax charged.",
  },
  {
    value: "DomesticReverseCharge",
    labelHu: "Belföldi fordított adózás",
    labelEn: "Domestic reverse charge",
    hintHu:
      "Belföldi fordított adózás [Áfa tv. 142. §]. A vevő önadózik; a soron 0% ÁFA.",
    hintEn: "Domestic reverse charge. The buyer self-assesses; 0% VAT on the line.",
  },
  {
    value: "IntraCommunityGoods",
    labelHu: "Közösségen belüli termékértékesítés (0%)",
    labelEn: "Intra-Community supply of goods (0%)",
    hintHu:
      "Közösségen belüli adómentes termékértékesítés [Áfa tv. 89. §] (KBAET). EU-s vevő, 0% ÁFA.",
    hintEn: "Intra-Community exempt supply of goods (KBAET). EU buyer, 0% VAT.",
  },
  {
    value: "IntraCommunityServiceReverse",
    labelHu: "Közösségen belüli szolgáltatás – fordított",
    labelEn: "Intra-Community service – reverse charge",
    hintHu:
      "Áfa területi hatályán kívüli, fordított adózású ügylet [Áfa tv. 37. §] (EUFAD37). Nem adómentesség — a teljesítés helye a vevő tagállama.",
    hintEn:
      "Out-of-scope, reverse-charged service (EUFAD37). Not an exemption — place of supply is the buyer's member state.",
  },
] as const;

/** ADR-0101 — does this VAT kind force the line VAT rate to 0 (and thus
 * lock the VAT% input)? True for every non-`Percent` kind: exempt /
 * reverse-charge / out-of-scope lines carry 0 VAT by law. This is the one
 * predicate the selector, the composer, and the vitest pin all share. */
export function vatKindLocksRate(kind: VatRateKindBody): boolean {
  return kind !== "Percent";
}

/** ADR-0102 — HÜLYE-BIZTOS cross-field guidance: does the selected VAT
 * kind require a different buyer type than the one currently chosen?
 * Returns a bilingual hint when there's a mismatch, or `null` when the
 * kind + buyer are compatible. The backend preflight is the hard gate
 * (`VatKindRequiresOtherBuyer` / `VatKindRequiresDomesticBuyer`); this
 * pure helper lets the form warn the operator inline BEFORE they submit,
 * so they don't assemble an invalid EU-0 + domestic-buyer invoice.
 *
 *   IntraCommunity* (KBAET/EUFAD37) → REQUIRES an `Other` (EU) buyer.
 *   AamExempt / DomesticReverseCharge → REQUIRES a `Domestic` buyer.
 *   Percent → any buyer (no hint). */
export function vatKindBuyerMismatch(
  kind: VatRateKindBody,
  vatStatus: CustomerVatStatusBody,
): { hu: string; en: string } | null {
  const needsOther =
    kind === "IntraCommunityGoods" || kind === "IntraCommunityServiceReverse";
  const needsDomestic =
    kind === "AamExempt" || kind === "DomesticReverseCharge";
  if (needsOther && vatStatus !== "Other") {
    return {
      hu: "Ez a Közösségen belüli 0%-os ÁFA-típus külföldi EU-s (Külföldi) vevőt igényel EU adószámmal. Válasszon EU-s vevőt, vagy módosítsa az ÁFA-típust.",
      en: "This intra-Community 0% VAT type requires a foreign-EU (Other) buyer with an EU VAT number. Pick an EU buyer, or change the VAT type.",
    };
  }
  if (needsDomestic && vatStatus !== "Domestic") {
    return {
      hu: "Ez a belföldi ÁFA-típus belföldi (Adóalany) vevőt igényel. Válasszon belföldi vevőt, vagy módosítsa az ÁFA-típust.",
      en: "This domestic VAT type requires a Domestic buyer. Pick a domestic buyer, or change the VAT type.",
    };
  }
  return null;
}

/** ADR-0101 — apply an operator VAT-kind pick to one line, HÜLYE-BIZTOS.
 * Pure — returns a fresh form; no side effects (mirror of
 * [`applyProductPick`] so vitest can pin the lock without mounting the
 * component). Switching TO a non-`Percent` kind forces `vatRatePercent` to
 * 0 (the invalid-combination guard: an exempt / reverse-charge line cannot
 * carry a rate). Switching BACK to `Percent` restores the Hungarian
 * standard 27% so the operator is not stranded on the 0 the lock left
 * behind — they can still type any standard rate afterwards.
 *
 * The component's `<select>` onchange calls this so the rate-lock invariant
 * lives in exactly one place (the composer also force-zeroes on the wire as
 * defence in depth). */
export function applyVatKind(
  form: IssueInvoiceFormState,
  lineIndex: number,
  kind: VatRateKindBody,
): IssueInvoiceFormState {
  const lines = form.lines.map((line, i) => {
    if (i !== lineIndex) return line;
    if (vatKindLocksRate(kind)) {
      return { ...line, vatRateKind: kind, vatRatePercent: 0 };
    }
    // Back to Percent — restore the standard rate if the lock had zeroed it.
    return {
      ...line,
      vatRateKind: kind,
      vatRatePercent: line.vatRatePercent === 0 ? 27 : line.vatRatePercent,
    };
  });
  return { ...form, lines };
}

/** S406 — resolve which seller-bank id the form should hold for a
 * given invoice currency, preserving an operator's explicit pick.
 *
 * Returns the `is_default` bank for `currency` when the operator has
 * not already chosen a bank in that currency (current pick is unknown
 * or belongs to a different currency); returns the operator's current
 * pick unchanged when it already matches the currency; returns `null`
 * when no bank is configured for the currency (the form then renders
 * the "no bank for currency" affordance + blocks Submit via
 * [`cannotIssueDueToBank`]).
 *
 * Pure — so vitest can pin the auto-flip's bank half without mounting
 * the component. The component's currency-change effect folds reactive
 * state through this and is the SOLE writer of `form.bankAccountId` on
 * a currency change (whether the change came from the dropdown or from
 * [`applyProductPick`]'s auto-flip). */
export function resolveBankForCurrency(
  banks: SellerBankResponse[],
  currency: Currency,
  currentBankId: string | null,
): string | null {
  const defaultForCurrency =
    banks.find((b) => b.currency === currency && b.is_default) ?? null;
  if (defaultForCurrency === null) return null;
  const current = banks.find((b) => b.id === currentBankId);
  if (current === undefined || current.currency !== currency) {
    return defaultForCurrency.id;
  }
  return currentBankId;
}

/** S406 — derived per-line currency-mismatch warning. Returns the data
 * for the warning chip when this line was autofilled from a product
 * whose default currency differs from the CURRENT invoice currency
 * (i.e. the operator overrode the auto-flipped currency); returns
 * `null` otherwise (one-off line, or product currency matches).
 *
 * [[trust-code-not-operator]] — the warning is derived from the wire
 * shape (the line's stamped `productCurrency` + the seller-bank list),
 * never from operator memory or a dismissable snapshot. The named
 * bank is the default seller bank for the PRODUCT's currency — the one
 * that "may not apply" because the invoice now bills in a different
 * currency. `null` `productCurrencyBankName` when no bank is
 * configured for the product's currency. */
export interface LineCurrencyMismatch {
  productName: string;
  productCurrency: Currency;
  invoiceCurrency: Currency;
  /** `bank_name` of the default seller bank for the PRODUCT's
   * currency, or `null` if none is configured. */
  productCurrencyBankName: string | null;
}

export function lineCurrencyMismatchWarning(args: {
  productCurrency: Currency | null | undefined;
  invoiceCurrency: Currency;
  productName: string;
  banks: SellerBankResponse[];
}): LineCurrencyMismatch | null {
  const { productCurrency, invoiceCurrency, productName, banks } = args;
  if (
    productCurrency === null ||
    productCurrency === undefined ||
    productCurrency === invoiceCurrency
  ) {
    return null;
  }
  const bankForProductCurrency =
    banks.find((b) => b.currency === productCurrency && b.is_default) ?? null;
  return {
    productName,
    productCurrency,
    invoiceCurrency,
    productCurrencyBankName: bankForProductCurrency?.bank_name ?? null,
  };
}

/** PR-84 — bidirectional payment-deadline helper. Given the form's
 * current `invoiceDate` and a new `offsetDays` value the operator just
 * typed, return the resolved absolute `paymentDeadline`. Returns null
 * on malformed input. The companion direction (operator picks an
 * absolute date) just sets `paymentDeadline` directly; the offset is
 * a derived read via `daysBetween(invoiceDate, paymentDeadline)`. */
export function paymentDeadlineFromOffset(
  invoiceDate: IsoDate,
  offsetDays: number,
): IsoDate | null {
  return addDays(invoiceDate, offsetDays);
}

/** PR-84 — classify a candidate delivery date against the form's
 * comfort zone [invoiceDate, paymentDeadline] and return the audit-
 * wire discriminant the form should stamp. `null` means in-range (no
 * audit flag, no confirm prompt needed); non-null means the operator
 * picked an out-of-range date and the form should surface the inline
 * "Are you sure?" confirm before stamping this on the wire body.
 *
 * Returns `null` on malformed input — the form's per-field validator
 * surfaces the precise gap separately. */
export function deliveryDateOverrideFor(
  invoiceDate: IsoDate,
  paymentDeadline: IsoDate,
  deliveryDate: IsoDate,
): DeliveryDateOverride {
  const zone = comfortZone(invoiceDate, paymentDeadline, deliveryDate);
  if (zone === null) return null;
  return overrideKindForZone(zone);
}

/** PR-50 / session-70 — typed `missing_seller_config` error body the
 * backend's `serve::handle_issue_invoice` 400 surface emits when
 * `validate_supplier_info` rejects the operator-typed tax number.
 * Mirrors `serve::TypedErrorBody` on the Rust side.
 *
 * The SPA's inline-error renderer detects this discriminant and
 * surfaces the `config_path` + `sample_path` as actionable hints so
 * the operator knows where the eventual config home lives (PR-51's
 * wizard destination) without having to dig through the close-handoff
 * notes. */
export interface MissingSellerConfigError {
  /** Discriminant — exact string the backend serializes. */
  error: "missing_seller_config";
  /** Human-readable diagnostic carrying the rejected input + the
   * shape expectation. Surfaced verbatim by the renderer. */
  message: string;
  /** Per-tenant `seller.toml` path the SPA shows as the "fill in
   * here" pointer. PR-51 wires this destination; today the message
   * still names it as the forward-looking config home. */
  config_path: string;
  /** Repo-relative `samples/seller.toml.example` path the SPA shows
   * as the template source. */
  sample_path: string;
}

/** PR-50 / session-70 — parse the raw error string the Tauri forward
 * helper hands back (shape:
 * `"backend returned 400 Bad Request for /invoices/issue: {json}"`)
 * into the typed `missing_seller_config` body when present.
 *
 * Returns `null` for any other shape (network error, 500, 400 without
 * the typed discriminant). The caller falls back to displaying the
 * raw message in that case.
 *
 * Hand-rolled JSON extraction (substring + JSON.parse) rather than
 * pulling in a parser dep — the wrapping format is fixed and the
 * `{ ... }` substring is unambiguous (the backend's body is a JSON
 * object). */
export function parseMissingSellerConfigError(
  raw: string,
): MissingSellerConfigError | null {
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start < 0 || end <= start) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.slice(start, end + 1));
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const obj = parsed as Record<string, unknown>;
  if (obj.error !== "missing_seller_config") return null;
  if (
    typeof obj.message !== "string" ||
    typeof obj.config_path !== "string" ||
    typeof obj.sample_path !== "string"
  ) {
    return null;
  }
  return {
    error: "missing_seller_config",
    message: obj.message,
    config_path: obj.config_path,
    sample_path: obj.sample_path,
  };
}

/** PR-69 / session-91 — closed-vocab pre-issuance error variant the
 * backend's `validate_invoice_preflight` enumerates per ADR-0038.
 * Mirrors the `kind` field of `serve::PreflightErrorItem` on the
 * Rust side. New variant requires a paired pin: extend this union
 * AND add a vitest case in `issue-invoice.test.ts`. */
export type InvoicePreflightErrorKind =
  | "CustomerNameEmpty"
  | "CustomerTaxNumberMissing"
  | "CustomerTaxNumberMalformed"
  // Session-150 — §169 buyer-address gate. Was missing from the
  // front-end closed-vocab, so a backend `CustomerAddressMissing`
  // collapsed the entire preflight parse to `null` and the operator saw
  // no §169 chip. Now exhaustive again per the union's invariant.
  | "CustomerAddressMissing"
  // PR-97 / ADR-0048 — reachable via the PrivatePerson / foreign-buyer
  // paths the session-150 address gate now exercises. Added alongside
  // CustomerAddressMissing to restore the documented exhaustive vocab.
  | "CustomerTaxNumberPresentForPrivatePerson"
  // ADR-0102 — EU-partner (Other) customer type + cross-field VAT
  // matrix. The Other buyer requires a valid communityVatNumber and no
  // HU tax number; the intra-Community 0% kinds require an Other buyer
  // and the domestic kinds require a Domestic buyer.
  | "CustomerTaxNumberPresentForOther"
  | "CommunityVatNumberMissing"
  | "CommunityVatNumberMalformed"
  | "VatKindRequiresOtherBuyer"
  | "VatKindRequiresDomesticBuyer"
  | "InvoiceLinesEmpty"
  | "LineItemDescriptionEmpty"
  | "LineItemQuantityZero"
  | "LineItemUnitPriceNonPositive"
  | "LineItemVatRateUnknown"
  // ADR-0101 (Session 2) — the per-line VAT rate-kind gate. All three route
  // to `lines[N].vatRateKind` (the selector). `NonZeroPercentForExemptKind`
  // fires only for a non-UI caller (the selector locks the rate);
  // `VatRateKindNotSupportedYet` rejects a named-deferred kind;
  // `MixedVatRateKindsUnsupported` rejects an invoice mixing kinds (the NAV
  // summary is single-bucket). New variant → extend this union AND add a
  // vitest case, same closed-vocab discipline as the rest.
  | "NonZeroPercentForExemptKind"
  | "VatRateKindNotSupportedYet"
  | "MixedVatRateKindsUnsupported"
  | "SellerBankMissingForCurrency"
  | "SellerBankCurrencyMismatch";

/** PR-69 / session-91 — one operator-correctable preflight error
 * returned by `POST /invoices/issue` when the request body fails the
 * pre-issuance shape gate (ADR-0038). The SPA's IssueInvoice form
 * renders these inline at `field_path`'s input (red border + the
 * Hungarian + English message stacked beneath the input). */
export interface InvoicePreflightErrorItem {
  /** Closed-vocab discriminant. The renderer pattern-matches on this
   * for variant-specific UI affordances (e.g. linking the rejected
   * VAT rate to the allowed-set hint). */
  kind: InvoicePreflightErrorKind;
  /** Dotted path into the wire shape (`customer.name`,
   * `lines[2].vatRatePercent`, …). Used to route the inline error to
   * the right input element. */
  field_path: string;
  /** Hungarian operator-facing message — rendered verbatim. */
  message_hu: string;
  /** English developer / debug message — rendered alongside HU. */
  message_en: string;
}

/** PR-69 / session-91 — typed 400 body the backend's
 * `serve::handle_issue_invoice` emits when the preflight validator
 * (`validate_invoice_preflight`, ADR-0038) returns a non-empty error
 * vec. Sibling of [`MissingSellerConfigError`] with a `errors` array
 * instead of a single message so the operator sees every problem at
 * once.
 *
 * The outer `error` discriminant distinguishes a preflight 400 from
 * the PR-50 `missing_seller_config` 400 and from the legacy plain
 * 400 (`validate_issue_request`'s empty-string surface). */
export interface InvoicePreflightErrorBody {
  error: "invoice_preflight_failed";
  errors: InvoicePreflightErrorItem[];
}

/** PR-69 / session-91 — parse the raw error string the Tauri
 * forward helper hands back (shape: `"backend returned 400 Bad
 * Request for /invoices/issue: {json}"`) into the typed preflight
 * body when present.
 *
 * Returns `null` for any other shape (network error, 500, plain 400
 * without the typed discriminant, `missing_seller_config` 400). The
 * caller then either tries `parseMissingSellerConfigError` (which
 * has the same return-null-on-mismatch posture) or falls back to
 * the raw message.
 *
 * Same hand-rolled JSON extraction as `parseMissingSellerConfigError`
 * — substring + JSON.parse, no dep. */
export function parseInvoicePreflightErrors(
  raw: string,
): InvoicePreflightErrorBody | null {
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start < 0 || end <= start) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.slice(start, end + 1));
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const obj = parsed as Record<string, unknown>;
  if (obj.error !== "invoice_preflight_failed") return null;
  if (!Array.isArray(obj.errors)) return null;
  const items: InvoicePreflightErrorItem[] = [];
  for (const candidate of obj.errors) {
    if (typeof candidate !== "object" || candidate === null) return null;
    const item = candidate as Record<string, unknown>;
    if (
      typeof item.kind !== "string" ||
      typeof item.field_path !== "string" ||
      typeof item.message_hu !== "string" ||
      typeof item.message_en !== "string"
    ) {
      return null;
    }
    if (!isKnownPreflightKind(item.kind)) return null;
    items.push({
      kind: item.kind,
      field_path: item.field_path,
      message_hu: item.message_hu,
      message_en: item.message_en,
    });
  }
  return { error: "invoice_preflight_failed", errors: items };
}

/** PR-69 / session-91 — closed-vocab guard. A backend variant the SPA
 * does not know about should fail loud rather than render as
 * `(unknown error)` — the renderer needs to know about every variant
 * so the inline-error UI is exhaustive. */
function isKnownPreflightKind(s: string): s is InvoicePreflightErrorKind {
  switch (s) {
    case "CustomerNameEmpty":
    case "CustomerTaxNumberMissing":
    case "CustomerTaxNumberMalformed":
    case "CustomerAddressMissing":
    case "CustomerTaxNumberPresentForPrivatePerson":
    case "CustomerTaxNumberPresentForOther":
    case "CommunityVatNumberMissing":
    case "CommunityVatNumberMalformed":
    case "VatKindRequiresOtherBuyer":
    case "VatKindRequiresDomesticBuyer":
    case "InvoiceLinesEmpty":
    case "LineItemDescriptionEmpty":
    case "LineItemQuantityZero":
    case "LineItemUnitPriceNonPositive":
    case "LineItemVatRateUnknown":
    case "NonZeroPercentForExemptKind":
    case "VatRateKindNotSupportedYet":
    case "MixedVatRateKindsUnsupported":
    case "SellerBankMissingForCurrency":
    case "SellerBankCurrencyMismatch":
      return true;
    default:
      return false;
  }
}

/** PR-69 / session-91 — given a `field_path` returned by the backend
 * preflight, extract a stable DOM-input identifier the IssueInvoice
 * form uses to target the inline-error rendering. Customer paths
 * map to bare field names; line paths to a `(lineIndex, field)`
 * tuple.
 *
 * Returns `null` for any path shape outside the closed-vocab — the
 * renderer then renders the error in the general error block rather
 * than dropping it. Same posture as the closed-vocab kind guard
 * above. */
export type PreflightFieldTarget =
  | {
      kind: "customer";
      // ADR-0102 — `communityVatNumber` (the EU VAT input) + `vatStatus`
      // (the buyer-type control the cross-field matrix errors route to).
      field:
        | "name"
        | "taxNumber"
        | "address"
        | "communityVatNumber"
        | "vatStatus";
    }
  | { kind: "lines" }
  | { kind: "bankAccountId" }
  | {
      kind: "line";
      lineIndex: number;
      field:
        | "description"
        | "quantity"
        | "unitPrice"
        | "vatRatePercent"
        // ADR-0101 — the per-line VAT-type selector; the three kind-level
        // preflight errors route here.
        | "vatRateKind";
    };

export function targetForFieldPath(
  fieldPath: string,
): PreflightFieldTarget | null {
  if (fieldPath === "customer.name") {
    return { kind: "customer", field: "name" };
  }
  if (fieldPath === "customer.taxNumber") {
    return { kind: "customer", field: "taxNumber" };
  }
  // Session-150 — route the §169 buyer-address error to the address
  // field group so it renders inline (bilingual) beneath the address
  // inputs rather than dropping into the unrouted catch-all (which
  // shows the HU message only).
  if (fieldPath === "customer.address") {
    return { kind: "customer", field: "address" };
  }
  // ADR-0102 — the EU community VAT input + the buyer-type control (the
  // cross-field kind↔buyer mismatches route to `vatStatus`).
  if (fieldPath === "customer.communityVatNumber") {
    return { kind: "customer", field: "communityVatNumber" };
  }
  if (fieldPath === "customer.vatStatus") {
    return { kind: "customer", field: "vatStatus" };
  }
  if (fieldPath === "lines") {
    return { kind: "lines" };
  }
  if (fieldPath === "bankAccountId") {
    return { kind: "bankAccountId" };
  }
  // Match `lines[N].field` where N is a non-negative integer and
  // field is one of the line-level closed-vocab field names (ADR-0101
  // adds `vatRateKind`).
  const lineMatch = /^lines\[(\d+)\]\.(description|quantity|unitPrice|vatRatePercent|vatRateKind)$/.exec(
    fieldPath,
  );
  if (lineMatch) {
    return {
      kind: "line",
      lineIndex: Number(lineMatch[1]),
      field: lineMatch[2] as
        | "description"
        | "quantity"
        | "unitPrice"
        | "vatRatePercent"
        | "vatRateKind",
    };
  }
  return null;
}

/** PR-44ζ — turn the form state into the wire `IssueInvoiceRequest`.
 * Pure function; no side effects. The trim on string fields mirrors
 * the backend's `validate_issue_request` (which `.trim()`-checks the
 * same fields) so a form value of `"   "` surfaces as a 400 with the
 * actionable "required" message rather than passing pre-validation
 * and failing deeper. */
export function composeIssueInvoiceBody(
  form: IssueInvoiceFormState,
): IssueInvoiceRequest {
  return {
    customer: {
      // PR-97 / ADR-0048 — closed-vocab buyer kind on the wire. The
      // backend's serde deserialiser defaults missing to `Domestic`
      // for back-compat, but the SPA always emits an explicit value
      // so the operator's radio choice rides on the audit trail.
      vatStatus: form.customerVatStatus,
      // PR-97 / ADR-0048 (Ervin override 1) — saved-partner id when
      // the operator picked a buyer via the typeahead. Backend uses
      // it to increment the partner's `issued_invoice_count` for the
      // PartnerForm's field-selective lock.
      partnerId: form.customerPartnerId,
      // PR-97 / ADR-0048 — for PrivatePerson buyers, the disabled
      // input emits `""`. Trim + leave empty so the backend's
      // preflight sees the empty-string signal and treats it
      // consistently with the wire-defaulted `Domestic` case (its
      // CustomerTaxNumberPresentForPrivatePerson gate fires only on
      // a non-empty value).
      taxNumber: form.customerTaxNumber.trim(),
      // ADR-0102 — snapshot the EU community VAT number onto the wire
      // body for Other (foreign-EU) buyers only. Trim; omit (undefined)
      // for non-Other or when blank so the backend's serde default
      // (None → Domestic path) keeps every non-Other body byte-identical.
      communityVatNumber:
        form.customerVatStatus === "Other" &&
        form.customerCommunityVatNumber.trim().length > 0
          ? form.customerCommunityVatNumber.trim()
          : undefined,
      name: form.customerName.trim(),
      // PR-77 / session-101 — customer address quartet. Always emit
      // the field when ANY of the four sub-strings is non-empty after
      // trim; the backend preflight rejects partially-blank addresses
      // explicitly so the operator sees the precise gap. If every
      // sub-string is blank we omit the field — that surfaces as
      // `CustomerAddressMissing` on the preflight (Domestic) or no-op
      // for PrivatePerson (NAV wire permits absence).
      address: composeCustomerAddress(form),
    },
    lines: form.lines.map((l) => ({
      description: l.description.trim(),
      // S157 — parse the operator-typed quantity (`1.5` or `1,5`) into the
      // canonical dot-decimal string. A parse failure (blank, zero,
      // negative, garbage) sends `"0"` so the backend preflight's
      // `LineItemQuantityZero` renders the inline error at this line's
      // quantity input — same posture as the unit-price `?? 0` below.
      quantity: parseDecimalQuantity(l.quantityInput) ?? "0",
      // PR-88 / session-113 — parse the operator-typed string into
      // integer minor units using the form's currency. Bare ints
      // are interpreted as WHOLE major units (`340` EUR = 34000
      // cents; `340` HUF = 340 forints). A parse failure surfaces
      // as 0 on the wire so the backend preflight's
      // `LineItemUnitPriceNonPositive` rule renders the inline
      // error at this line's unit-price input — the operator gets
      // the existing PR-69 actionable message rather than a silent
      // bad-amount issuance. See [`parseAmountToMinor`] for the
      // closed grammar.
      unitPrice: parseAmountToMinor(l.unitPriceInput, form.currency) ?? 0,
      // ADR-0101 — a non-`Percent` kind's line VAT is always 0. Force it
      // here (defence in depth) so a UI-lock bypass or a stale
      // `vatRatePercent` can never send a nonzero rate for an exempt /
      // reverse-charge line (the backend also rejects that via
      // `NonZeroPercentForExemptKind`, but the composer should never emit
      // the contradiction in the first place).
      vatRatePercent: vatKindLocksRate(l.vatRateKind) ? 0 : l.vatRatePercent,
      // ADR-0101 — the operator's VAT-type choice, sent verbatim. Absent →
      // backend defaults `"Percent"`; the SPA always emits it explicitly.
      vatRateKind: l.vatRateKind,
      // PR-82 — per-line buyer note. Trim + normalise empty to
      // `null` so the backend's preflight / persistence path sees a
      // clean "no note" signal rather than a blank-string row.
      note: blankToNull(l.note),
      // S159 — the picked product's unit (PR-100 picker). `null` for
      // one-off freetext lines; the backend's NAV emit falls back to
      // PIECE for a null unit. Emitted verbatim — the backend's
      // `LineJson.unit: Option<ProductUnit>` deserialises null as None.
      unit: l.unit ?? null,
    })),
    currency: form.currency,
    // PR-73 / ADR-0040 §addendum — operator-selected bank account.
    // Sent verbatim; `null` lets the backend fall back to the per-
    // currency default. Empty-string is normalised to `null` so the
    // backend resolver sees a clean "no selection" signal.
    bankAccountId:
      form.bankAccountId !== null && form.bankAccountId.trim() !== ""
        ? form.bankAccountId
        : null,
    // PR-82 — per-invoice global buyer note. Same blank-to-null
    // normalisation as per-line notes; the backend's `Option<String>`
    // deserialiser treats `null` and an absent field identically.
    invoiceNote: blankToNull(form.invoiceNote),
    // PR-84 — operator-supplied payment deadline + delivery date go
    // on the wire verbatim. Both are canonical YYYY-MM-DD strings.
    // The wire body does NOT carry the form's `invoiceDate` field —
    // the server stamps the immutable issue date from its own clock
    // (per ADR-0007 §"Operator-as-threat-actor"); the form's display
    // value is purely UX-anchoring.
    paymentDeadline: form.paymentDeadline,
    deliveryDate: form.deliveryDate,
    // PR-84 — comfort-zone audit discriminant. `null` for in-range
    // (default operator path, no audit flag); a non-null value
    // travels into the backend's `InvoiceDraftCreated` audit payload
    // verbatim. The composer does NOT re-classify here — the SPA's
    // Svelte component owns the operator's confirm UX and writes the
    // discriminant only after the operator has confirmed the out-of-
    // range choice. The backend independently re-classifies via
    // `aberp_billing::classify_delivery_date` for defence in depth.
    deliveryDateOverride: form.deliveryDateOverride,
    // PR-92 / ADR-0047 — operator-typed default-on toggle. The wire
    // body carries the exact bool the operator set; the backend
    // defaults to `true` when absent (defence in depth — a future
    // composer regression that drops this field still produces the
    // default-on behaviour).
    emailBuyerOnIssue: form.emailBuyerOnIssue,
    // PR-99 Item 4 Part B — mirror posture for the auto-submit-to-NAV
    // toggle. Same default-true semantics on both ends; the backend's
    // `submit_to_nav_on_issue.unwrap_or(true)` mirrors `email_buyer`.
    submitToNavOnIssue: form.submitToNavOnIssue,
    // S160 / ADR-0050 — operator's payment-method dropdown choice, sent
    // verbatim as the bare NAV token. The backend snapshots it on the
    // invoice and emits `<paymentMethod>`; absent → `TRANSFER` default.
    paymentMethod: form.paymentMethod,
    // PR-203 / S203 — per-invoice email recipient override. Blank-after-
    // trim becomes `null` so the backend resolver sees the "no override,
    // fall back to partner.email" signal verbatim (matches the existing
    // `blankToNull` posture for invoiceNote / per-line note channels).
    emailRecipientOverride: blankToNull(form.emailRecipientOverride),
  };
}

/** PR-82 — trim + normalise a form-supplied note string to `null`
 * when blank, `string` otherwise. Centralised so the per-line and
 * per-invoice note channels share one rule (empty-after-trim ⇒
 * `null`). The backend's note channel is `Option<String>`; passing
 * `Some("")` would be wire-confusing and litter the DuckDB column
 * with empty strings that the renderer would then filter out anyway. */
function blankToNull(raw: string | null | undefined): string | null {
  if (raw === null || raw === undefined) return null;
  const trimmed = raw.trim();
  return trimmed === "" ? null : trimmed;
}

/** PR-77 / session-101 — build the customer-address body shape from
 * the form's four address fields. Returns `undefined` (omitting the
 * wire field) when every field is blank-after-trim so the backend's
 * preflight emits the cleaner `CustomerAddressMissing` message rather
 * than rejecting a body with four empty strings. Otherwise returns
 * the trimmed quartet verbatim — partially-blank shapes flow through
 * because the per-field preflight gate names the precise gap, not a
 * generic "address is malformed" lump. */
export function composeCustomerAddress(
  form: IssueInvoiceFormState,
): { countryCode: string; postalCode: string; city: string; street: string } | undefined {
  const countryCode = form.customerCountryCode.trim();
  const postalCode = form.customerPostalCode.trim();
  const city = form.customerCity.trim();
  const street = form.customerStreet.trim();
  if (
    countryCode === "" &&
    postalCode === "" &&
    city === "" &&
    street === ""
  ) {
    return undefined;
  }
  return { countryCode, postalCode, city, street };
}

/** PR-75 / session-99 — inputs to the Submit-button gate for the
 * bank-picker branch. Pure data; no Svelte runes — so vitest can pin
 * the gate decision without mounting `IssueInvoice.svelte`. */
export interface IssueSubmitGateInputs {
  /** `true` once `loadSellerBanks()` has resolved (success OR caught
   * failure). `false` while the request is in flight. */
  sellerBanksLoaded: boolean;
  /** Non-null when `loadSellerBanks()` rejected. The error message the
   * SPA surfaces inline; presence alone is the gate signal. */
  sellerBanksLoadError: string | null;
  /** Number of bank entries whose currency matches the form's currency.
   * Zero means "no bank account configured for this currency" — the
   * issuance path cannot complete without one. */
  banksForCurrencyCount: number;
}

/** PR-75 / session-99 — closes the live-test regression Ervin caught:
 * clicking "Issue invoice" when no bank entry exists for the form's
 * currency fired a silent request that produced no inline feedback
 * (the backend's bank resolver loud-failed, but the SPA route had no
 * affordance for the bank-missing class of error). Pre-PR-75 the
 * button was always enabled.
 *
 * Returns `true` iff the bank picker is unresolvable; the Svelte
 * component then disables `<button type="submit">` so the operator
 * sees a clearer dead-end (the "no bank for currency" hint above the
 * button + the disabled state) instead of a click that does nothing.
 *
 * Three failure modes, any one of which gates the button:
 *   1. Banks haven't loaded yet (`!sellerBanksLoaded`).
 *   2. Banks load FAILED (`sellerBanksLoadError !== null`).
 *   3. Banks loaded but there are zero entries for the form's current
 *      currency (`banksForCurrencyCount === 0`). */
export function cannotIssueDueToBank(args: IssueSubmitGateInputs): boolean {
  return (
    !args.sellerBanksLoaded ||
    args.sellerBanksLoadError !== null ||
    args.banksForCurrencyCount === 0
  );
}
