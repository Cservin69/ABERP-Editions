//! PR-69 / session-91 — pre-issuance validation for the operator-typed
//! per-invoice fields on `POST /invoices/issue`. Tier-1 UX item #4.
//!
//! The route handler runs [`validate_invoice_preflight`] BEFORE the
//! issuance pipeline so an operator-correctable problem (empty
//! customer name, malformed ADÓSZÁM, zero quantity, off-vocab VAT
//! rate) surfaces as a typed 400 the SPA's IssueInvoice form renders
//! inline per field — instead of round-tripping to NAV and discovering
//! the problem hours later at submit time. Mirrors PR-50's
//! supplier-config gate (the `missing_seller_config` typed 400) at the
//! per-invoice-field level.
//!
//! # Scope (ADR-0038 §Decision)
//!
//! Eight variants covering only fields that exist on today's wire
//! shape `serve::IssueInvoiceRequest`:
//!
//!   * Customer name + tax number shape (`customer.name`,
//!     `customer.taxNumber`).
//!   * Lines non-empty + per-line description / quantity / unit price
//!     / VAT rate (against the Hungarian Áfa standard-rate vocab
//!     `{0, 5, 18, 27}`).
//!
//! Variants the brief enumerated that CANNOT fire from today's wire
//! shape (no address fields, no per-line currency, no operator-typed
//! issue date, no due date, no SPA-declared totals) are named-deferred
//! in ADR-0038 §"Open questions" with explicit triggers — F12-style
//! discipline for "scope grows when the wire shape does."
//!
//! # Posture (CLAUDE.md rules 7 + 12 + 13)
//!
//!   * Pure function; no I/O, no DB, no network. Testable under
//!     `cargo test --lib` without fixtures.
//!   * Returns `Vec<InvoicePreflightError>` so the operator sees every
//!     error at once instead of one-per-resubmit (mirror of
//!     `setup_seller_info::FieldError` collection per PR-51).
//!   * Closed-vocab + deny-default: a ninth variant requires explicit
//!     enum addition AND a Vitest+Rust pin pair (CLAUDE.md rule 9).
//!   * Hungarian + English messages: operator base is Hungarian
//!     (`project_aberp_ui_milestone` posture), English is the
//!     developer / debug fallback. Both surfaced on every variant so
//!     the SPA renders verbatim — translation duplication and drift
//!     stay off the table.

use aberp_billing::{Currency, VatRateKind};
use rust_decimal::Decimal;

use crate::nav_xml::{
    parse_hungarian_tax_number, validate_community_vat_number, validate_country_code,
    CustomerVatStatus, SupplierConfigError,
};
use crate::serve::IssueInvoiceRequest;

/// Hungarian Áfa standard-rate closed-vocab. Per current Áfa törvény:
/// 27% (general), 18% (reduced), 5% (further reduced), 0%. Special
/// non-numeric categories (AAM / TAM / TAH) use a different wire-shape
/// surface (`vatRateType`) that today's `LineJson.vat_rate_percent`
/// (u16) does not carry; widening to those categories is named-deferred
/// per ADR-0038 §"Open questions".
pub const ALLOWED_VAT_RATES_PERCENT: &[u16] = &[0, 5, 18, 27];

/// Wire-shape discriminant for the typed 400 body. Surfaced verbatim
/// in the response's `error` field; the SPA's
/// `parseInvoicePreflightErrors` pattern-matches on this string to
/// distinguish a preflight 400 from a plain 400 (e.g. the legacy
/// `validate_issue_request` empty-string surface) or the PR-50
/// `missing_seller_config` 400.
pub const ERR_INVOICE_PREFLIGHT_FAILED: &str = "invoice_preflight_failed";

/// Closed-vocab pre-issuance error per ADR-0038. Variants pin distinct
/// failure modes so the SPA's inline-error renderer can target the
/// offending input by `field_path` (dotted path into the wire shape;
/// for line errors uses `lines[N].field` indexing).
///
/// Adding a variant requires:
///   * Updating [`InvoicePreflightError::kind`] / `field_path` /
///     `message_hu` / `message_en` accessors.
///   * One Rust pin (`#[test] fn variant_X_fires_for_Y`).
///   * One Vitest pin (`it("renders inline at field_path for variant
///     X")`).
///
/// The four-edit discipline mirrors F12 — drift between the variant
/// list and either the accessors or the test surface fails loud at
/// commit time (CLAUDE.md rule 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvoicePreflightError {
    /// `customer.name.trim().is_empty()`.
    CustomerNameEmpty,
    /// `customer.tax_number.trim().is_empty()`.
    CustomerTaxNumberMissing,
    /// `customer.tax_number` fails `parse_hungarian_tax_number` shape
    /// validation. Carries the raw input + the static reason from the
    /// parser so the operator sees exactly what was rejected.
    CustomerTaxNumberMalformed {
        actual: String,
        reason: &'static str,
    },
    /// PR-77 / session-101 + session-150 — `customer.address` absent
    /// (or any of its four sub-fields blank). Fires for ALL customer
    /// types: for Domestic it is additionally gated on a well-formed
    /// tax number (the malformed case is a more proximate fix); for
    /// PrivatePerson it fires unconditionally per Áfa tv. §169 (the
    /// printed/PDF invoice mandates the buyer address for every customer
    /// type — ADR-0048 amendment 2026-05-29 reverts the PR-97 override-2
    /// carve-out). Originally PR-77 also prevented the NAV-side
    /// `CUSTOMER_DATA_EXPECTED` ABORTED that burned invoice 18; that
    /// protection is preserved for Domestic. See
    /// [`customer_address_complete`].
    ///
    /// Surfaced as the typed-400 wire body the SPA's
    /// `parseInvoicePreflightErrors` consumes; the SPA's combobox-
    /// driven IssueInvoice form populates `customer.address` from the
    /// operator-selected partner record, so a fire on this variant
    /// usually means the operator picked a partner with an incomplete
    /// address — the fix is in Partners (enrich the partner record),
    /// not in the issuance form.
    CustomerAddressMissing,
    /// `lines.is_empty()`. Lifted from the legacy
    /// `validate_issue_request` surface into the closed-vocab so the
    /// SPA's per-field renderer can target `lines` rather than
    /// rendering a string blob.
    InvoiceLinesEmpty,
    /// `lines[line_index].description.trim().is_empty()`.
    LineItemDescriptionEmpty { line_index: usize },
    /// `lines[line_index].quantity <= 0`. S157 — quantity is `Decimal`;
    /// the gate now rejects zero AND negative (the SPA composer maps an
    /// unparseable input to `"0"` so this fires for blank/garbage too).
    LineItemQuantityZero { line_index: usize },
    /// `lines[line_index].unit_price <= 0`. An invoice with a zero or
    /// negative unit price is not a meaningful business document; a
    /// credit-note line uses the storno chain (PR-10 + PR-11), not a
    /// negative line on an issue.
    LineItemUnitPriceNonPositive { line_index: usize, actual: i64 },
    /// `lines[line_index].vat_rate_percent` not in
    /// [`ALLOWED_VAT_RATES_PERCENT`]. The loud-fail message names the
    /// rejected value AND the allowed set so the operator who
    /// legitimately needs a non-standard rate has a precise pointer at
    /// what to argue for (the trigger for closed-vocab widening per
    /// ADR-0038 §"Open questions").
    LineItemVatRateUnknown {
        line_index: usize,
        actual: u16,
        allowed: &'static [u16],
    },
    /// ADR-0101 §4 (Session 2 — DOOR OPEN). `lines[line_index]` carries a
    /// FULLY-WIRED non-`Percent` kind (AAM / domestic reverse-charge /
    /// intra-Community goods / intra-Community service reverse) but a
    /// **non-zero** `vat_rate_percent`. A reverse-charge / exempt line's
    /// VAT is always 0 (the buyer self-assesses; the exemption carries no
    /// rate); a non-zero rate on such a line is a category/rate
    /// contradiction NAV would reject. The SPA selector LOCKS the rate to 0
    /// for these kinds, so this fires only for a non-UI caller (CLI / API)
    /// — a defence-in-depth gate, not an operator-reachable state through
    /// the form. Carries the kind + the offending rate so the message names
    /// both.
    NonZeroPercentForExemptKind {
        line_index: usize,
        kind: VatRateKind,
        actual: u16,
    },
    /// ADR-0101 §4 — `lines[line_index]` carries a NAMED-DEFERRED
    /// `vat_rate_kind` (TAM / EAM / NAM / KBAUK / HO / marginScheme /
    /// noVatCharge / vatContent). These are enum-known so the vocab is
    /// closed + complete-in-intent, but v1 wires only the four Ervin-
    /// confirmed kinds; the remainder are explicit not-yet markers
    /// (ADR-0048 pattern / CLAUDE.md rule 12). Rejected LOUDLY at preflight
    /// (and, in defence-in-depth, `anyhow!`ed at NAV emit) so no
    /// unconfirmed-code body can reach the wire. Wiring one is a separate
    /// PR that first confirms its NAV case code.
    VatRateKindNotSupportedYet {
        line_index: usize,
        kind: VatRateKind,
    },
    /// ADR-0101 (Session-2 conservative guard — FLAGGED). The invoice mixes
    /// a non-`Percent` `vat_rate_kind` with a DIFFERENT kind on another
    /// line. The NAV emitter's `summaryByVatRate` is single-bucket, keyed
    /// on the FIRST line's kind/rate (`nav_xml.rs::write_summary`), so a
    /// mixed-kind invoice would emit a summary whose category does not
    /// cover every line — a NAV cross-field rejection at best, a silently
    /// mis-categorised filing at worst. Until the summary emitter groups by
    /// (kind, rate) — a separate follow-up — an invoice that carries ANY
    /// non-`Percent` line must have EVERY line on that same kind. All-
    /// `Percent` invoices are unaffected (byte-identical backward-compat);
    /// this guard fires only once a non-`Percent` kind is present. Carries
    /// the two disagreeing kinds + the line that first differs.
    ///
    /// FLAG (for the NAV-category adversarial review): this enforces the
    /// invariant `write_summary` already assumes but never checked. The
    /// pre-existing mixed-`Percent`-RATE single-bucket gap (e.g. a 27% line
    /// + a 5% line) is untouched here — out of ADR-0101 scope — and remains
    /// a NAV-reject-on-submit rather than a preflight catch.
    MixedVatRateKindsUnsupported {
        line_index: usize,
        first_kind: VatRateKind,
        other_kind: VatRateKind,
    },
    /// PR-73 / ADR-0040 §addendum — `bank_account_id` omitted (or
    /// `None`) AND `seller_banks.default_bank_for(invoice.currency)`
    /// returned `None`. The operator must either add a bank-account
    /// entry for the invoice's currency in Tenant Settings or pick an
    /// explicit `bank_account_id` of a different currency (the latter
    /// will then trip the `SellerBankCurrencyMismatch` variant below,
    /// which carries the same message-payload contract).
    ///
    /// Field-path: `bankAccountId` (closed-vocab camelCase). The SPA's
    /// inline-error renderer targets the bank-picker dropdown by this
    /// path; the renderer also surfaces a "navigate to Tenant
    /// Settings" affordance because this variant is the one case where
    /// the fix is NOT in the form but in a sibling screen.
    SellerBankMissingForCurrency { currency: Currency },
    /// PR-73 / ADR-0040 §addendum — operator explicitly selected a
    /// `bank_account_id` whose `currency` does not match the invoice's
    /// currency. Same field-path as `SellerBankMissingForCurrency`
    /// (`bankAccountId`) so the SPA's inline-error renderer targets
    /// the same dropdown.
    ///
    /// Carries the selected id + its currency + the invoice's
    /// currency so the bilingual message names all three values — the
    /// operator either rotated the SPA-side dropdown filter (a stale
    /// id from a different-currency cache) or the dropdown drift-bug
    /// surfaced; both fix paths are operator-resubmit.
    SellerBankCurrencyMismatch {
        selected_id: String,
        selected_currency: Currency,
        invoice_currency: Currency,
    },
    /// PR-97 / ADR-0048 §6 — `customer.vat_status == PrivatePerson`
    /// AND `customer.tax_number` is non-empty after trim. NAV's
    /// business-rule layer rejects PRIVATE_PERSON + `<customerVatData>`
    /// (the symmetric negative half of `CUSTOMER_DATA_EXPECTED`);
    /// rather than burn a sequence and discover the rejection at
    /// submit time, surface it inline at preflight. Carries the
    /// rejected raw value so the operator sees exactly what the
    /// disabled input was carrying (usually a copy/paste residue
    /// after switching the radio).
    CustomerTaxNumberPresentForPrivatePerson { actual: String },
    /// ADR-0102 §4(b) — `customer.vat_status == Other` AND
    /// `customer.tax_number` is non-empty after trim. A foreign-EU
    /// buyer carries a `<communityVatNumber>`, NOT a Hungarian ADÓSZÁM;
    /// a HU tax number on an `Other` body is operator confusion (radio
    /// flipped after typing) or a wire-bypass. Symmetric with the
    /// PrivatePerson rule; surfaced loud so the operator clears the
    /// field rather than silently emitting an ignored value.
    CustomerTaxNumberPresentForOther { actual: String },
    /// ADR-0102 §4(b) — `customer.vat_status == Other` but no EU
    /// community VAT number was supplied. NAV's `CustomerVatDataType`
    /// REQUIRES `<customerVatData>` for OTHER (the EU arm carries
    /// `<communityVatNumber>`); an absent number would render an empty
    /// `<customerVatData>` that NAV rejects. Fix is on the partner
    /// record (fill the EU VAT number) or the buyer type.
    CommunityVatNumberMissing,
    /// ADR-0102 §3.3 — `customer.vat_status == Other` and the supplied
    /// EU community VAT number fails the VIES STRUCTURAL shape
    /// (unknown country prefix / bad length / non-alphanumeric body).
    /// Carries the raw value + the bilingual reason from
    /// `nav_xml::validate_community_vat_number`. (A live VIES online
    /// check is out of scope — ADR-0102 §8.2.)
    CommunityVatNumberMalformed { actual: String, reason: String },
    /// ADR-0102 (NAV-adversarial FIX #1) — `customer.vat_status == Other`
    /// and the buyer address `country_code` is not a well-formed ISO
    /// 3166-1 alpha-2 code (`[A-Z]{2}`). An `Other` buyer's country flows
    /// from the free-form partner record, so a partner saved as "Austria"
    /// / "Ausztria" (or a lowercase "at") would emit a `<countryCode>` NAV
    /// bounces at submit time. Surfaced loud at preflight (symmetric with
    /// `CommunityVatNumberMalformed`) so the operator fixes the partner's
    /// country on the editable form rather than burning a sequence at NAV.
    /// Fires ONLY for `Other` (Domestic buyers are HU-forced by the SPA,
    /// out of scope — backward-compat). Carries the raw value + the
    /// bilingual reason from `nav_xml::validate_country_code`. A full
    /// closed-vocab country list stays deferred (ADR-0102 §8.3).
    CustomerCountryCodeInvalid { actual: String, reason: String },
    /// ADR-0102 §4(a) — `lines[line_index]` carries an intra-Community
    /// 0% kind (`IntraCommunityGoods`/KBAET or
    /// `IntraCommunityServiceReverse`/EUFAD37) but the buyer is NOT
    /// `Other` (foreign-EU business). An EU-0 line is only well-formed
    /// against a foreign-EU buyer with a community VAT number; assembling
    /// one against a domestic buyer is the exact "semantically-wrong
    /// buyer" compliance error this ADR closes. Carries the kind + the
    /// actual buyer status so the message names both. Fix: pick an
    /// `Other` (EU) buyer, or change the line's VAT type.
    VatKindRequiresOtherBuyer {
        line_index: usize,
        kind: VatRateKind,
        actual_status: CustomerVatStatus,
    },
    /// ADR-0102 §4(a) — `lines[line_index]` carries `DomesticReverseCharge`
    /// but the buyer is NOT `Domestic`. `belföldi fordított adózás` (§142)
    /// is two domestic taxable persons by definition, so this kind is
    /// Domestic-only. NOTE (NAV-adversarial FIX #2, 2026-07-16): `AamExempt`
    /// USED to also require Domestic here, but AAM (alanyi adómentesség,
    /// §187) is a SELLER-side exemption — NAV binds NO customerVatStatus to
    /// it — so the strict rule wrongly blocked the common exempt-small-
    /// business → magánszemély case. AAM is now buyer-agnostic (like
    /// `Percent`) and never reaches this variant. Carries the kind + the
    /// actual buyer status.
    VatKindRequiresDomesticBuyer {
        line_index: usize,
        kind: VatRateKind,
        actual_status: CustomerVatStatus,
    },
}

impl InvoicePreflightError {
    /// Closed-vocab discriminant — the variant name verbatim. The SPA
    /// renderer switches on this string to route to the right input
    /// (alongside `field_path`).
    pub fn kind(&self) -> &'static str {
        match self {
            InvoicePreflightError::CustomerNameEmpty => "CustomerNameEmpty",
            InvoicePreflightError::CustomerTaxNumberMissing => "CustomerTaxNumberMissing",
            InvoicePreflightError::CustomerTaxNumberMalformed { .. } => {
                "CustomerTaxNumberMalformed"
            }
            InvoicePreflightError::CustomerAddressMissing => "CustomerAddressMissing",
            InvoicePreflightError::InvoiceLinesEmpty => "InvoiceLinesEmpty",
            InvoicePreflightError::LineItemDescriptionEmpty { .. } => "LineItemDescriptionEmpty",
            InvoicePreflightError::LineItemQuantityZero { .. } => "LineItemQuantityZero",
            InvoicePreflightError::LineItemUnitPriceNonPositive { .. } => {
                "LineItemUnitPriceNonPositive"
            }
            InvoicePreflightError::LineItemVatRateUnknown { .. } => "LineItemVatRateUnknown",
            InvoicePreflightError::NonZeroPercentForExemptKind { .. } => {
                "NonZeroPercentForExemptKind"
            }
            InvoicePreflightError::VatRateKindNotSupportedYet { .. } => {
                "VatRateKindNotSupportedYet"
            }
            InvoicePreflightError::MixedVatRateKindsUnsupported { .. } => {
                "MixedVatRateKindsUnsupported"
            }
            InvoicePreflightError::SellerBankMissingForCurrency { .. } => {
                "SellerBankMissingForCurrency"
            }
            InvoicePreflightError::SellerBankCurrencyMismatch { .. } => {
                "SellerBankCurrencyMismatch"
            }
            InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson { .. } => {
                "CustomerTaxNumberPresentForPrivatePerson"
            }
            InvoicePreflightError::CustomerTaxNumberPresentForOther { .. } => {
                "CustomerTaxNumberPresentForOther"
            }
            InvoicePreflightError::CommunityVatNumberMissing => "CommunityVatNumberMissing",
            InvoicePreflightError::CommunityVatNumberMalformed { .. } => {
                "CommunityVatNumberMalformed"
            }
            InvoicePreflightError::CustomerCountryCodeInvalid { .. } => {
                "CustomerCountryCodeInvalid"
            }
            InvoicePreflightError::VatKindRequiresOtherBuyer { .. } => "VatKindRequiresOtherBuyer",
            InvoicePreflightError::VatKindRequiresDomesticBuyer { .. } => {
                "VatKindRequiresDomesticBuyer"
            }
        }
    }

    /// Dotted path into the wire shape (`customer.name`,
    /// `lines[2].vatRatePercent`, …). The SPA's renderer maps this to
    /// the offending input element. camelCase to match the JSON wire
    /// field names.
    pub fn field_path(&self) -> String {
        match self {
            InvoicePreflightError::CustomerNameEmpty => "customer.name".to_string(),
            InvoicePreflightError::CustomerTaxNumberMissing
            | InvoicePreflightError::CustomerTaxNumberMalformed { .. } => {
                "customer.taxNumber".to_string()
            }
            InvoicePreflightError::CustomerAddressMissing => "customer.address".to_string(),
            InvoicePreflightError::InvoiceLinesEmpty => "lines".to_string(),
            InvoicePreflightError::LineItemDescriptionEmpty { line_index } => {
                format!("lines[{line_index}].description")
            }
            InvoicePreflightError::LineItemQuantityZero { line_index } => {
                format!("lines[{line_index}].quantity")
            }
            InvoicePreflightError::LineItemUnitPriceNonPositive { line_index, .. } => {
                format!("lines[{line_index}].unitPrice")
            }
            InvoicePreflightError::LineItemVatRateUnknown { line_index, .. } => {
                format!("lines[{line_index}].vatRatePercent")
            }
            // All three ADR-0101 kind-level errors route to the per-line
            // `vatRateKind` selector so the SPA renders them beneath the
            // VAT-type dropdown (the control that drives the discriminant).
            InvoicePreflightError::NonZeroPercentForExemptKind { line_index, .. }
            | InvoicePreflightError::VatRateKindNotSupportedYet { line_index, .. }
            | InvoicePreflightError::MixedVatRateKindsUnsupported { line_index, .. } => {
                format!("lines[{line_index}].vatRateKind")
            }
            InvoicePreflightError::SellerBankMissingForCurrency { .. }
            | InvoicePreflightError::SellerBankCurrencyMismatch { .. } => {
                "bankAccountId".to_string()
            }
            InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson { .. }
            | InvoicePreflightError::CustomerTaxNumberPresentForOther { .. } => {
                "customer.taxNumber".to_string()
            }
            InvoicePreflightError::CommunityVatNumberMissing
            | InvoicePreflightError::CommunityVatNumberMalformed { .. } => {
                "customer.communityVatNumber".to_string()
            }
            InvoicePreflightError::CustomerCountryCodeInvalid { .. } => {
                "customer.address.countryCode".to_string()
            }
            // The cross-field kind↔buyer mismatches are triggered by a
            // line's VAT type but fixed on the buyer side; route them to
            // the buyer-type control so the operator sees "this line kind
            // needs a different buyer".
            InvoicePreflightError::VatKindRequiresOtherBuyer { .. }
            | InvoicePreflightError::VatKindRequiresDomesticBuyer { .. } => {
                "customer.vatStatus".to_string()
            }
        }
    }

    /// Hungarian operator-facing message. Surfaced verbatim by the
    /// SPA's inline-error renderer.
    pub fn message_hu(&self) -> String {
        match self {
            InvoicePreflightError::CustomerNameEmpty => {
                "A vevő neve kötelező a számlán (Áfa tv. §169)".to_string()
            }
            InvoicePreflightError::CustomerTaxNumberMissing => {
                "Az ügyfél adószáma (ADÓSZÁM) kötelező (helyes: `xxxxxxxx-y-zz`, pl. `87654321-2-13`)."
                    .to_string()
            }
            InvoicePreflightError::CustomerTaxNumberMalformed { actual, reason } => {
                format!(
                    "Az ügyfél adószáma (`{actual}`) hibás formátum ({}). Helyes: `xxxxxxxx-y-zz`, pl. `87654321-2-13`.",
                    translate_reason_hu(reason)
                )
            }
            InvoicePreflightError::CustomerAddressMissing => {
                "A vevő címe kötelező a számlán (Áfa tv. §169) — pótold a partner adatlapján \
                 (ország, irányítószám, város, utca)."
                    .to_string()
            }
            InvoicePreflightError::InvoiceLinesEmpty => {
                "Legalább egy tételsor szükséges a számlához.".to_string()
            }
            InvoicePreflightError::LineItemDescriptionEmpty { line_index } => {
                format!(
                    "A(z) {}. tételsor megnevezése kötelező.",
                    line_index + 1
                )
            }
            InvoicePreflightError::LineItemQuantityZero { line_index } => {
                // S157 — fractional quantities are valid; the gate is
                // "strictly positive" (nullánál nagyobb), not "≥ 1".
                format!(
                    "A(z) {}. tételsor mennyisége nullánál nagyobb kell legyen.",
                    line_index + 1
                )
            }
            InvoicePreflightError::LineItemUnitPriceNonPositive { line_index, actual } => {
                format!(
                    "A(z) {}. tételsor egységára pozitív kell legyen (kapott: {actual}). Sztornó / módosítás külön folyamat.",
                    line_index + 1
                )
            }
            InvoicePreflightError::LineItemVatRateUnknown {
                line_index,
                actual,
                allowed,
            } => {
                format!(
                    "A(z) {}. tételsor ÁFA-kulcsa ({actual}%) nem szerepel a magyar szabványos kulcsok között ({}). Adómentes / fordított adózású tételhez válassza a megfelelő ÁFA-típust (AAM, belföldi fordított adózás, Közösségen belüli) a legördülő menüből.",
                    line_index + 1,
                    format_percent_list(allowed)
                )
            }
            InvoicePreflightError::NonZeroPercentForExemptKind {
                line_index,
                kind,
                actual,
            } => {
                format!(
                    "A(z) {}. tételsor ÁFA-típusa ({}) adómentes / fordított adózású, ezért az ÁFA-kulcs kötelezően 0% (kapott: {actual}%). A vevő önadózik / az ügylet adómentes — állítsa 0%-ra.",
                    line_index + 1,
                    kind.as_str()
                )
            }
            InvoicePreflightError::VatRateKindNotSupportedYet { line_index, kind } => {
                format!(
                    "A(z) {}. tételsor ÁFA-típusa ({}) még nincs bekötve (ADR-0101 named-deferred). Jelenleg a Százalékos (Percent), AAM, belföldi fordított adózás, valamint a Közösségen belüli termékértékesítés / szolgáltatás típusok bocsáthatók ki. Válasszon ezek közül.",
                    line_index + 1,
                    kind.as_str()
                )
            }
            InvoicePreflightError::MixedVatRateKindsUnsupported {
                line_index,
                first_kind,
                other_kind,
            } => {
                format!(
                    "A(z) {}. tételsor ÁFA-típusa ({}) eltér a számla első tételsorának típusától ({}). Egy adómentes / fordított adózású számlán minden tételsornak azonos ÁFA-típusúnak kell lennie (a NAV összesítő egyetlen kategóriát tartalmaz). Bontsa külön számlára a különböző ÁFA-típusú tételeket.",
                    line_index + 1,
                    other_kind.as_str(),
                    first_kind.as_str()
                )
            }
            InvoicePreflightError::SellerBankMissingForCurrency { currency } => {
                format!(
                    "Nincs konfigurált bankszámla a számla pénzneméhez ({}). Adjon meg egy `[[seller.banks]]` bejegyzést ehhez a pénznemhez a Bérlőbeállítások / Bank accounts menüpontban.",
                    currency.iso_code()
                )
            }
            InvoicePreflightError::SellerBankCurrencyMismatch {
                selected_id,
                selected_currency,
                invoice_currency,
            } => {
                format!(
                    "A választott bankszámla (`{selected_id}`) pénzneme {} eltér a számla pénznemétől {}. Válasszon olyan bankszámlát, amelynek pénzneme megegyezik a számla pénznemével.",
                    selected_currency.iso_code(),
                    invoice_currency.iso_code()
                )
            }
            InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson { actual } => {
                format!(
                    "Magánszemély vevőhöz nem tartozhat adószám (kapott: `{actual}`). \
                     Természetes személy vevő esetén a NAV szabálya tiltja a `<customerVatData>` \
                     blokkot — váltson Adóalany típusra, vagy hagyja üresen az ADÓSZÁM mezőt."
                )
            }
            InvoicePreflightError::CustomerTaxNumberPresentForOther { actual } => {
                format!(
                    "Külföldi (OTHER) vevőhöz nem tartozhat magyar adószám (kapott: `{actual}`). \
                     Az EU-s vevő EU-adószámot (communityVatNumber) visel — hagyja üresen az \
                     ADÓSZÁM mezőt."
                )
            }
            InvoicePreflightError::CommunityVatNumberMissing => {
                "Külföldi (OTHER) vevőhöz kötelező az EU adószám (communityVatNumber). \
                 Töltse ki a partner „EU VAT number\" mezőjét, vagy váltson vevőtípust."
                    .to_string()
            }
            InvoicePreflightError::CommunityVatNumberMalformed { actual, reason } => {
                format!(
                    "Az EU adószám `{actual}` nem érvényes alakú: {reason}. \
                     (Csak alaki ellenőrzés — élő VIES ellenőrzés nem történik.)"
                )
            }
            InvoicePreflightError::CustomerCountryCodeInvalid { actual, reason } => {
                format!(
                    "A külföldi (OTHER) vevő országkódja `{actual}` nem érvényes: {reason}. \
                     Javítsa a partner országát ISO 3166-1 alpha-2 kódra (pl. AT, DE) — \
                     a NAV a kétbetűs kódot várja."
                )
            }
            InvoicePreflightError::VatKindRequiresOtherBuyer { line_index, kind, actual_status } => {
                format!(
                    "A(z) {}. sor ÁFA-típusa ({kind:?}) Közösségen belüli 0%-os ügylet, \
                     amely külföldi EU-s (OTHER) vevőt igényel — a jelenlegi vevő {}. \
                     Válasszon EU-s vevőt (EU adószámmal), vagy módosítsa a sor ÁFA-típusát.",
                    line_index + 1,
                    actual_status.as_nav_token()
                )
            }
            InvoicePreflightError::VatKindRequiresDomesticBuyer { line_index, kind, actual_status } => {
                format!(
                    "A(z) {}. sor ÁFA-típusa ({kind:?}) belföldi ügylet, amely belföldi \
                     (DOMESTIC) vevőt igényel — a jelenlegi vevő {}. Válasszon belföldi vevőt, \
                     vagy módosítsa a sor ÁFA-típusát.",
                    line_index + 1,
                    actual_status.as_nav_token()
                )
            }
        }
    }

    /// English developer / debug message. Surfaced verbatim by the
    /// SPA's inline-error renderer alongside the Hungarian message.
    pub fn message_en(&self) -> String {
        match self {
            InvoicePreflightError::CustomerNameEmpty => "Buyer name required per §169".to_string(),
            InvoicePreflightError::CustomerTaxNumberMissing => {
                "Customer ADÓSZÁM is required (expected `xxxxxxxx-y-zz`, e.g. `87654321-2-13`)."
                    .to_string()
            }
            InvoicePreflightError::CustomerTaxNumberMalformed { actual, reason } => {
                format!(
                    "Customer ADÓSZÁM `{actual}` is not a valid Hungarian tax number ({reason}); expected `xxxxxxxx-y-zz`, e.g. `87654321-2-13`."
                )
            }
            InvoicePreflightError::CustomerAddressMissing => {
                "Buyer address required per §169 — fix the partner record \
                 (country, postal code, city, street)."
                    .to_string()
            }
            InvoicePreflightError::InvoiceLinesEmpty => {
                "At least one line item is required.".to_string()
            }
            InvoicePreflightError::LineItemDescriptionEmpty { line_index } => {
                format!("Line {} description is required.", line_index + 1)
            }
            InvoicePreflightError::LineItemQuantityZero { line_index } => {
                // S157 — fractional quantities are now valid (1.5 days,
                // 0.25 hours); the gate is "strictly positive", not
                // "integer ≥ 1".
                format!(
                    "Line {} quantity must be greater than zero.",
                    line_index + 1
                )
            }
            InvoicePreflightError::LineItemUnitPriceNonPositive { line_index, actual } => {
                format!(
                    "Line {} unit price must be positive (got {actual}). Storno / modification is a separate flow.",
                    line_index + 1
                )
            }
            InvoicePreflightError::LineItemVatRateUnknown {
                line_index,
                actual,
                allowed,
            } => {
                format!(
                    "Line {} VAT rate ({actual}%) is not a Hungarian standard rate (allowed: {}). For an exempt / reverse-charge line, pick the matching VAT type (AAM, domestic reverse-charge, intra-Community) from the VAT-type selector instead.",
                    line_index + 1,
                    format_percent_list(allowed)
                )
            }
            InvoicePreflightError::NonZeroPercentForExemptKind {
                line_index,
                kind,
                actual,
            } => {
                format!(
                    "Line {} VAT kind ({}) is exempt / reverse-charge, so its VAT rate must be 0% (got {actual}%). The buyer self-assesses / the supply is exempt — set the rate to 0.",
                    line_index + 1,
                    kind.as_str()
                )
            }
            InvoicePreflightError::VatRateKindNotSupportedYet { line_index, kind } => {
                format!(
                    "Line {} VAT kind ({}) is not wired yet (ADR-0101 named-deferred). Issuable kinds today: Percent, AamExempt, DomesticReverseCharge, IntraCommunityGoods, IntraCommunityServiceReverse — pick one of those. Wiring another is a later PR that first confirms its NAV case code.",
                    line_index + 1,
                    kind.as_str()
                )
            }
            InvoicePreflightError::MixedVatRateKindsUnsupported {
                line_index,
                first_kind,
                other_kind,
            } => {
                format!(
                    "Line {} VAT kind ({}) differs from the invoice's first line kind ({}). An invoice carrying any exempt / reverse-charge line must have EVERY line on the same VAT kind — NAV's summaryByVatRate is single-bucket, so a mixed-kind invoice mis-categorises the summary. Split differing VAT kinds onto separate invoices.",
                    line_index + 1,
                    other_kind.as_str(),
                    first_kind.as_str()
                )
            }
            InvoicePreflightError::SellerBankMissingForCurrency { currency } => {
                format!(
                    "No bank account configured for the invoice's currency ({}). Add a `[[seller.banks]]` entry for this currency in Tenant Settings → Bank accounts.",
                    currency.iso_code()
                )
            }
            InvoicePreflightError::SellerBankCurrencyMismatch {
                selected_id,
                selected_currency,
                invoice_currency,
            } => {
                format!(
                    "Selected bank account (`{selected_id}`) currency {} does not match the invoice currency {}. Pick a bank account whose currency matches the invoice.",
                    selected_currency.iso_code(),
                    invoice_currency.iso_code()
                )
            }
            InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson { actual } => {
                format!(
                    "Natural-person (PRIVATE_PERSON) buyers must NOT carry a tax number \
                     (got `{actual}`). NAV's business-rule layer forbids `<customerVatData>` \
                     under PRIVATE_PERSON — switch the buyer type to Domestic or clear the \
                     ADÓSZÁM field."
                )
            }
            InvoicePreflightError::CustomerTaxNumberPresentForOther { actual } => {
                format!(
                    "Foreign-EU (OTHER) buyers must NOT carry a Hungarian tax number \
                     (got `{actual}`). An EU buyer carries an EU VAT number \
                     (communityVatNumber) — clear the ADÓSZÁM field."
                )
            }
            InvoicePreflightError::CommunityVatNumberMissing => {
                "Foreign-EU (OTHER) buyers require an EU VAT number (communityVatNumber). \
                 Fill the partner's `EU VAT number` field, or change the buyer type."
                    .to_string()
            }
            InvoicePreflightError::CommunityVatNumberMalformed { actual, reason } => {
                format!(
                    "EU VAT number `{actual}` is not structurally valid: {reason}. \
                     (Structural check only — no live VIES lookup is performed.)"
                )
            }
            InvoicePreflightError::CustomerCountryCodeInvalid { actual, reason } => {
                format!(
                    "Foreign-EU (OTHER) buyer country code `{actual}` is not valid: {reason}. \
                     Fix the partner's country to an ISO 3166-1 alpha-2 code (e.g. AT, DE) — \
                     NAV requires the two-letter code."
                )
            }
            InvoicePreflightError::VatKindRequiresOtherBuyer {
                line_index,
                kind,
                actual_status,
            } => {
                format!(
                    "Line {} VAT type ({kind:?}) is an intra-Community 0% transaction that \
                     requires a foreign-EU (OTHER) buyer — the current buyer is {}. Pick an EU \
                     buyer (with an EU VAT number), or change the line's VAT type.",
                    line_index + 1,
                    actual_status.as_nav_token()
                )
            }
            InvoicePreflightError::VatKindRequiresDomesticBuyer {
                line_index,
                kind,
                actual_status,
            } => {
                format!(
                    "Line {} VAT type ({kind:?}) is a domestic transaction that requires a \
                     Domestic buyer — the current buyer is {}. Pick a domestic buyer, or change \
                     the line's VAT type.",
                    line_index + 1,
                    actual_status.as_nav_token()
                )
            }
        }
    }
}

impl std::fmt::Display for InvoicePreflightError {
    /// Display falls back to the English message so an
    /// `anyhow::Result` chain naming a preflight error reads cleanly in
    /// logs and tests.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message_en())
    }
}

impl std::error::Error for InvoicePreflightError {}

/// Render `[0, 5, 18, 27]` as `"0%, 5%, 18%, 27%"` for the
/// operator-facing message. Kept private — the wire body emits
/// `allowed` as a JSON array so the SPA renderer can format
/// independently.
fn format_percent_list(rates: &[u16]) -> String {
    rates
        .iter()
        .map(|r| format!("{r}%"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Translate the English `&'static str` reason from
/// `SupplierConfigError::MalformedTaxNumber` into Hungarian for the
/// `message_hu` output. Kept as a small lookup rather than expanding
/// `SupplierConfigError` because the parser's reason vocabulary is
/// closed and small (six strings); a future widening lands the
/// Hungarian text alongside the English `&'static str`.
fn translate_reason_hu(reason: &str) -> &'static str {
    match reason {
        "expected three dash-separated segments" => {
            "három, kötőjellel elválasztott szegmens szükséges"
        }
        "taxpayerId segment must be exactly 8 digits" => {
            "az adózói azonosító szegmens pontosan 8 számjegy"
        }
        "vatCode segment must be exactly 1 digit" => "az ÁFA-kód szegmens pontosan 1 számjegy",
        "countyCode segment must be exactly 2 digits" => "a megyekód szegmens pontosan 2 számjegy",
        "taxpayerId segment must be ASCII digits only" => {
            "az adózói azonosító csak számjegyeket tartalmazhat"
        }
        "vatCode segment must be an ASCII digit" => "az ÁFA-kód csak számjegy lehet",
        "countyCode segment must be ASCII digits only" => {
            "a megyekód csak számjegyeket tartalmazhat"
        }
        _ => "hibás formátum",
    }
}

/// Session-150 — a buyer address is "complete" for invoicing iff all
/// four structured sub-fields are non-empty after trim. Shared by the
/// Domestic and PrivatePerson preflight arms: Áfa tv. §169 mandates the
/// buyer address on the printed/PDF invoice for ALL customer types
/// (PR-104 / ADR-0048 amendment 2026-05-29 reverts the PR-97 override-2
/// carve-out). Postal code is required — NOT relaxed — because every
/// v1-supported buyer is Hungarian (the `Other` foreign-buyer branch is
/// named-deferred) and a HU address always carries an irányítószám;
/// NAV's `<common:simpleAddress>` XSD also rejects an empty
/// `<postalCode>` on the wire, so allowing it here would re-introduce
/// the invoice-18 sequence-burn (PR-77).
fn customer_address_complete(address: &Option<crate::issue_invoice::AddressJson>) -> bool {
    address.as_ref().is_some_and(|a| {
        !a.country_code.trim().is_empty()
            && !a.postal_code.trim().is_empty()
            && !a.city.trim().is_empty()
            && !a.street.trim().is_empty()
    })
}

/// PR-69 / session-91 — pure-fn pre-issuance validator per ADR-0038.
/// Returns ALL errors in one pass so the operator can fix every issue
/// at once instead of discovering them one-per-resubmit. Pure: no I/O,
/// no DB, no network — safe to call from the route handler before any
/// state-changing work.
///
/// Caller is the `serve::handle_issue_invoice` route handler; if the
/// returned vec is non-empty the handler emits the typed 400 body
/// (`{error: "invoice_preflight_failed", errors: [...]}`) and skips
/// the rest of the pipeline (no DB write, no audit entry, no NAV XML
/// render).
pub fn validate_invoice_preflight(request: &IssueInvoiceRequest) -> Vec<InvoicePreflightError> {
    let mut errors: Vec<InvoicePreflightError> = Vec::new();

    // Customer block.
    //
    // Session-148 (Ervin override 3) — the buyer name is now
    // UNCONDITIONALLY required for ALL customer types per Áfa tv. §169
    // (the PR-104 ADR-0048 amendment made §169 mandatory on the PDF for
    // every buyer kind; this finishes the cleanup). The PR-97 GDPR
    // carve-out that suppressed `CustomerNameEmpty` for PRIVATE_PERSON
    // is removed — "forget GDPR, show the name, always."
    if request.customer.name.trim().is_empty() {
        errors.push(InvoicePreflightError::CustomerNameEmpty);
    }

    // PR-97 / ADR-0048 §6 — preflight is conditional on the
    // closed-vocab buyer-kind discriminator. Domestic preserves the
    // PR-69 + PR-77 rules (tax number required & well-formed + full
    // address required). PrivatePerson inverts the tax-number rule
    // (must be empty) and relaxes the address rule (optional at the
    // NAV wire layer). Other surfaces a typed `not-yet-supported`
    // error so the v1-deferred branch is operator-visible rather
    // than silently broken at submit time.
    let tax_trimmed = request.customer.tax_number.trim();
    match request.customer.vat_status {
        CustomerVatStatus::Domestic => {
            let mut tax_number_well_formed = false;
            if tax_trimmed.is_empty() {
                errors.push(InvoicePreflightError::CustomerTaxNumberMissing);
            } else {
                match parse_hungarian_tax_number(tax_trimmed) {
                    Ok(_) => {
                        tax_number_well_formed = true;
                    }
                    Err(SupplierConfigError::MissingTaxNumber) => {
                        // Whitespace-after-trim case is already covered by
                        // the `is_empty` branch above; this arm is
                        // theoretically unreachable but the explicit match
                        // keeps the closed-vocab exhaustive.
                        errors.push(InvoicePreflightError::CustomerTaxNumberMissing);
                    }
                    Err(SupplierConfigError::MalformedTaxNumber { input, reason }) => {
                        errors.push(InvoicePreflightError::CustomerTaxNumberMalformed {
                            actual: input,
                            reason,
                        });
                    }
                }
            }
            // PR-77 / session-101 — `customer.address` is required when
            // the buyer is a Hungarian business AND the tax number is
            // well-formed. The malformed-tax-number case suppresses the
            // address gate so the operator has a more proximate problem
            // to fix first.
            if tax_number_well_formed && !customer_address_complete(&request.customer.address) {
                errors.push(InvoicePreflightError::CustomerAddressMissing);
            }
        }
        CustomerVatStatus::PrivatePerson => {
            // Symmetric invariant: a natural-person buyer MUST NOT
            // carry a tax number. The SPA's IssueInvoice form disables
            // the input under this radio, so a non-empty value reaching
            // preflight is either operator confusion (radio flipped
            // after typing the number) or a wire-bypass. Surface
            // typed so the operator can clear the field inline.
            if !tax_trimmed.is_empty() {
                errors.push(
                    InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson {
                        actual: tax_trimmed.to_string(),
                    },
                );
            }
            // Session-150 — Áfa tv. §169 mandates the buyer address on
            // the printed/PDF invoice for ALL customer types including
            // natural persons. The PR-97 / Ervin-override-2 carve-out
            // that let a PrivatePerson invoice issue with an
            // address-light partner record is REVERTED (ADR-0048
            // amendment 2026-05-29; same legal foundation as session-148
            // for the buyer name). The NAV WIRE layer still permits
            // address-absence under PRIVATE_PERSON — `write_customer`
            // only emits `<customerAddress>` when present — so this
            // preflight gate is the §169 enforcement point, not
            // nav_xml.rs (which is unchanged).
            if !customer_address_complete(&request.customer.address) {
                errors.push(InvoicePreflightError::CustomerAddressMissing);
            }
        }
        CustomerVatStatus::Other => {
            // ADR-0102 §4(b) — foreign-EU business buyer. NAV requires
            // `<customerVatData>` (the EU arm carries `<communityVatNumber>`)
            // AND `<customerAddress>` for OTHER (the `CUSTOMER_DATA_EXPECTED`
            // predicate is `status != PRIVATE_PERSON`). A HU ADÓSZÁM is
            // forbidden (an EU buyer carries an EU VAT number, not a HU one).
            if !tax_trimmed.is_empty() {
                errors.push(InvoicePreflightError::CustomerTaxNumberPresentForOther {
                    actual: tax_trimmed.to_string(),
                });
            }
            match request
                .customer
                .community_vat_number
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                None => errors.push(InvoicePreflightError::CommunityVatNumberMissing),
                Some(raw) => {
                    if let Err(reason) = validate_community_vat_number(raw) {
                        errors.push(InvoicePreflightError::CommunityVatNumberMalformed {
                            actual: raw.to_string(),
                            reason,
                        });
                    }
                }
            }
            // Address REQUIRED for OTHER (same `CUSTOMER_DATA_EXPECTED`
            // rule as Domestic — the predicate is `status != PRIVATE_PERSON`).
            if !customer_address_complete(&request.customer.address) {
                errors.push(InvoicePreflightError::CustomerAddressMissing);
            } else if let Some(addr) = request.customer.address.as_ref() {
                // ADR-0102 (NAV-adversarial FIX #1) — the address is present
                // and non-empty, but for an `Other` (foreign-EU) buyer the
                // `country_code` flows from the free-form partner record; NAV's
                // `CountryCodeType` is `[A-Z]{2}`, so a partner saved as
                // "Austria"/"Ausztria" would bounce at submit time. Guard the
                // structural shape here (verbatim — the emitter writes it
                // byte-for-byte). Only `Other` is checked; Domestic buyers are
                // HU-forced by the SPA and out of scope (backward-compat).
                if let Err(reason) = validate_country_code(&addr.country_code) {
                    errors.push(InvoicePreflightError::CustomerCountryCodeInvalid {
                        actual: addr.country_code.clone(),
                        reason,
                    });
                }
            }
        }
    }

    // Lines block.
    if request.lines.is_empty() {
        errors.push(InvoicePreflightError::InvoiceLinesEmpty);
    } else {
        for (line_index, line) in request.lines.iter().enumerate() {
            if line.description.trim().is_empty() {
                errors.push(InvoicePreflightError::LineItemDescriptionEmpty { line_index });
            }
            // S157 — quantity is `Decimal`; reject zero and negative (a
            // line must move strictly positive units). Fractional positive
            // values (1.5, 0.25) pass.
            if line.quantity <= Decimal::ZERO {
                errors.push(InvoicePreflightError::LineItemQuantityZero { line_index });
            }
            if line.unit_price <= 0 {
                errors.push(InvoicePreflightError::LineItemUnitPriceNonPositive {
                    line_index,
                    actual: line.unit_price,
                });
            }
            // ADR-0101 §4 — the per-line VAT rate-kind accept/reject matrix
            // (Session 2, DOOR OPEN). Branch on the kind FIRST so a
            // non-`Percent` line's (meaningless) `vat_rate_percent` cannot
            // ALSO trip `LineItemVatRateUnknown` — the operator sees the one
            // precise kind-level error.
            //
            //   Percent        → numeric-rate gate (unchanged): rate ∈ {0,5,18,27}
            //   wired non-Pct  → accept iff rate == 0, else NonZeroPercentForExemptKind
            //   named-deferred → VatRateKindNotSupportedYet (loud not-yet)
            if line.vat_rate_kind.is_percent() {
                if !ALLOWED_VAT_RATES_PERCENT.contains(&line.vat_rate_percent) {
                    errors.push(InvoicePreflightError::LineItemVatRateUnknown {
                        line_index,
                        actual: line.vat_rate_percent,
                        allowed: ALLOWED_VAT_RATES_PERCENT,
                    });
                }
            } else if line.vat_rate_kind.is_wired() {
                // AAM / domestic reverse-charge / intra-Community goods /
                // intra-Community service reverse: the line VAT amount is 0
                // (buyer self-assesses / exempt), so a non-zero rate is a
                // category/rate contradiction. Accept only rate 0.
                if line.vat_rate_percent != 0 {
                    errors.push(InvoicePreflightError::NonZeroPercentForExemptKind {
                        line_index,
                        kind: line.vat_rate_kind,
                        actual: line.vat_rate_percent,
                    });
                }
            } else {
                // Named-deferred remainder — enum-known, not yet wired.
                errors.push(InvoicePreflightError::VatRateKindNotSupportedYet {
                    line_index,
                    kind: line.vat_rate_kind,
                });
            }
        }

        // ADR-0101 conservative mixed-kind guard (FLAGGED). The NAV emitter's
        // `summaryByVatRate` is single-bucket, keyed on the FIRST line's kind
        // (`nav_xml.rs::write_summary`). Once ANY line carries a non-`Percent`
        // kind, every line must share that exact kind or the emitted summary
        // would not cover every line's category — a NAV cross-field mismatch.
        // All-`Percent` invoices skip this guard entirely (the `any`
        // short-circuits false), so backward-compat is byte-identical. The
        // pre-existing mixed-`Percent`-RATE single-bucket gap is out of
        // ADR-0101 scope and intentionally NOT gated here.
        let any_non_percent = request.lines.iter().any(|l| !l.vat_rate_kind.is_percent());
        if any_non_percent {
            let first_kind = request.lines[0].vat_rate_kind;
            for (line_index, line) in request.lines.iter().enumerate() {
                if line.vat_rate_kind != first_kind {
                    errors.push(InvoicePreflightError::MixedVatRateKindsUnsupported {
                        line_index,
                        first_kind,
                        other_kind: line.vat_rate_kind,
                    });
                }
            }
        }

        // ADR-0102 §4(a) — CROSS-FIELD VAT-kind ↔ buyer-status matrix.
        // The mixed-kind guard above already forces every non-`Percent`
        // line to share one kind, so the invoice's operative non-`Percent`
        // kind is the FIRST such line; checking it once against the
        // invoice-scoped customer status is sufficient (and avoids piling a
        // duplicate error on every line). This closes the "EU-0 line
        // assembled against a semantically-wrong domestic buyer" risk: the
        // ONLY accept path for an intra-Community 0% kind is an `Other`
        // (foreign-EU) buyer, and `DomesticReverseCharge` REQUIRES a
        // Domestic buyer.
        //
        // NAV-adversarial FIX #2 (2026-07-16) — `AamExempt` (AAM, alanyi
        // adómentesség §187) is now BUYER-AGNOSTIC, like `Percent`: AAM is a
        // SELLER-side exemption and NAV binds no customerVatStatus to it, so
        // the earlier "AAM requires Domestic" rule wrongly blocked the common
        // legitimate case (exempt small business → magánszemély /
        // PrivatePerson, or → EU buyer). Only `DomesticReverseCharge` stays
        // Domestic-only (§142 belföldi = two domestic taxable persons — that
        // one is genuinely buyer-constrained). `Percent` is buyer-agnostic
        // (ADR-0101 hold) and never reaches this branch.
        if let Some((line_index, line)) = request
            .lines
            .iter()
            .enumerate()
            .find(|(_, l)| !l.vat_rate_kind.is_percent())
        {
            let status = request.customer.vat_status;
            match line.vat_rate_kind {
                VatRateKind::IntraCommunityGoods | VatRateKind::IntraCommunityServiceReverse
                    if status != CustomerVatStatus::Other =>
                {
                    errors.push(InvoicePreflightError::VatKindRequiresOtherBuyer {
                        line_index,
                        kind: line.vat_rate_kind,
                        actual_status: status,
                    });
                }
                VatRateKind::DomesticReverseCharge if status != CustomerVatStatus::Domestic => {
                    errors.push(InvoicePreflightError::VatKindRequiresDomesticBuyer {
                        line_index,
                        kind: line.vat_rate_kind,
                        actual_status: status,
                    });
                }
                // Matching-buyer cases + `AamExempt` (buyer-agnostic per FIX #2)
                // fall through (accepted). Percent never reaches here (find
                // skips is_percent); named-deferred kinds were already rejected
                // loud above by `VatRateKindNotSupportedYet`.
                _ => {}
            }
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue_invoice::{AddressJson, CustomerJson, LineJson};
    use crate::nav_xml::CustomerVatStatus;
    use aberp_billing::Currency;

    /// PR-77 / session-101 — canonical good-buyer address fixture for
    /// the preflight unit tests. All four sub-fields populated so the
    /// `CustomerAddressMissing` gate stays quiet on the golden path.
    fn good_customer_address() -> AddressJson {
        AddressJson {
            country_code: "HU".to_string(),
            postal_code: "1052".to_string(),
            city: "Budapest".to_string(),
            street: "Váci utca 19.".to_string(),
        }
    }

    fn good_line() -> LineJson {
        LineJson {
            description: "CNC machining service".to_string(),
            quantity: Decimal::from(1),
            unit_price: 10_000,
            vat_rate_percent: 27,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        }
    }

    fn good_request() -> IssueInvoiceRequest {
        IssueInvoiceRequest {
            customer: CustomerJson {
                community_vat_number: None,
                // PR-97 / ADR-0048 — explicit Domestic preserves
                // pre-PR-97 implicit posture for legacy preflight tests.
                vat_status: CustomerVatStatus::Domestic,
                partner_id: None,
                tax_number: "87654321-2-13".to_string(),
                name: "Áben Consulting KFT.".to_string(),
                address: Some(good_customer_address()),
            },
            lines: vec![good_line()],
            currency: Currency::Huf,
            series: None,
            bank_account_id: None,
            invoice_note: None,
            // PR-84 — preflight covers customer / lines / bank only;
            // the three date fields are optional on the wire and the
            // issuance pipeline defaults them to the system issue date
            // when absent. The good-request fixture doesn't set them.
            payment_deadline: None,
            delivery_date: None,
            delivery_date_override: None,
            // PR-92 — opt-out the auto-send in preflight fixtures so
            // none of the preflight tests need a configured SMTP
            // tenant; the issue route's `unwrap_or(true)` default
            // still applies for production callers.
            email_buyer_on_issue: Some(false),
            // PR-99 Item 4 Part B — same opt-out posture for the
            // auto-submit-to-NAV toggle. Preflight unit tests do not
            // exercise the NAV submit path.
            submit_to_nav_on_issue: Some(false),
            // S160 — preflight fixtures use the default payment method.
            payment_method: aberp_billing::PaymentMethod::default(),
            email_recipient_override: None,
        }
    }

    // ── per-variant arms ───────────────────────────────────────────────

    #[test]
    fn golden_valid_request_returns_empty_vec() {
        assert!(validate_invoice_preflight(&good_request()).is_empty());
    }

    /// ADR-0101 §4 (DOOR OPEN) + ADR-0102 §4 — each of the four
    /// FULLY-WIRED kinds is ACCEPTED when the line is 0% AND the buyer
    /// matches the cross-field matrix (AAM / domestic reverse-charge
    /// require a Domestic buyer; the intra-Community 0% kinds require an
    /// `Other` (EU) buyer with a valid EU VAT number). Proven
    /// kind-by-kind so a regression that re-shut one specific kind — or
    /// broke its buyer-matrix cell — fails loudly.
    #[test]
    fn wired_kinds_accepted_at_zero_rate_with_matching_buyer() {
        use aberp_billing::VatRateKind::*;
        for kind in [
            AamExempt,
            DomesticReverseCharge,
            IntraCommunityGoods,
            IntraCommunityServiceReverse,
        ] {
            let mut r = good_request();
            r.lines[0].vat_rate_kind = kind;
            r.lines[0].vat_rate_percent = 0; // exempt / reverse-charge is 0%
                                             // ADR-0102 — the EU-0 kinds need an Other buyer; the domestic
                                             // kinds keep good_request's Domestic buyer.
            if matches!(kind, IntraCommunityGoods | IntraCommunityServiceReverse) {
                make_other_buyer(&mut r);
            }
            let errs = validate_invoice_preflight(&r);
            assert!(
                errs.is_empty(),
                "wired kind {kind:?} at 0% + matching buyer must be accepted; got {errs:?}"
            );
        }
    }

    /// ADR-0101 §4 — a wired kind with a NON-ZERO rate trips
    /// `NonZeroPercentForExemptKind` (and NOT the numeric `LineItemVatRateUnknown`
    /// — the kind branch is taken first, so the operator sees one precise
    /// error). A reverse-charge / exempt line's VAT is always 0.
    #[test]
    fn wired_kind_with_nonzero_rate_rejected_non_zero_percent() {
        use aberp_billing::VatRateKind::*;
        for kind in [
            AamExempt,
            DomesticReverseCharge,
            IntraCommunityGoods,
            IntraCommunityServiceReverse,
        ] {
            let mut r = good_request();
            r.lines[0].vat_rate_kind = kind;
            r.lines[0].vat_rate_percent = 27; // contradiction: exempt kind + rate
            let errs = validate_invoice_preflight(&r);
            assert!(
                errs.iter().any(|e| matches!(
                    e,
                    InvoicePreflightError::NonZeroPercentForExemptKind { kind: k, actual: 27, .. }
                        if *k == kind
                )),
                "wired kind {kind:?} + 27% must trip NonZeroPercentForExemptKind; got {errs:?}"
            );
            assert!(
                !errs
                    .iter()
                    .any(|e| matches!(e, InvoicePreflightError::LineItemVatRateUnknown { .. })),
                "the kind branch is taken first — LineItemVatRateUnknown must NOT co-fire; got {errs:?}"
            );
        }
    }

    /// ADR-0101 §4 — every NAMED-DEFERRED kind stays rejected (loud)
    /// `VatRateKindNotSupportedYet`, at any rate. This is the closed-vocab
    /// not-yet marker: no unconfirmed-code body reaches the wire.
    #[test]
    fn named_deferred_kinds_rejected_not_supported_yet() {
        use aberp_billing::VatRateKind::*;
        for kind in [
            TamExempt,
            ExportGoods,
            OtherInternational,
            NewTransportIntraCommunity,
            OutOfScopeThirdCountry,
            MarginScheme,
            NoVatCharge,
            VatContent,
        ] {
            for rate in [0u16, 27] {
                let mut r = good_request();
                r.lines[0].vat_rate_kind = kind;
                r.lines[0].vat_rate_percent = rate;
                let errs = validate_invoice_preflight(&r);
                assert!(
                    errs.iter().any(|e| matches!(
                        e,
                        InvoicePreflightError::VatRateKindNotSupportedYet { kind: k, .. }
                            if *k == kind
                    )),
                    "named-deferred kind {kind:?} @ {rate}% must be rejected VatRateKindNotSupportedYet; got {errs:?}"
                );
            }
        }
    }

    /// ADR-0101 conservative guard (FLAGGED) — an invoice mixing a
    /// non-`Percent` kind with a DIFFERENT kind trips
    /// `MixedVatRateKindsUnsupported` (the summary emitter is single-bucket).
    /// A Percent line beside an AAM line is a mix; two AAM lines are not.
    #[test]
    fn mixed_kinds_rejected_but_uniform_non_percent_accepted() {
        use aberp_billing::VatRateKind::*;
        // Percent line 0 + AAM line 1 → mix (line 1 differs from first).
        let mut mixed = good_request();
        mixed.lines = vec![good_line(), good_line()];
        mixed.lines[1].vat_rate_kind = AamExempt;
        mixed.lines[1].vat_rate_percent = 0;
        let errs = validate_invoice_preflight(&mixed);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                InvoicePreflightError::MixedVatRateKindsUnsupported {
                    line_index: 1,
                    first_kind: Percent,
                    other_kind: AamExempt,
                }
            )),
            "Percent + AAM mix must trip MixedVatRateKindsUnsupported at line 1; got {errs:?}"
        );

        // Two AAM lines → uniform, accepted (no mix error).
        let mut uniform = good_request();
        uniform.lines = vec![good_line(), good_line()];
        for l in uniform.lines.iter_mut() {
            l.vat_rate_kind = AamExempt;
            l.vat_rate_percent = 0;
        }
        let errs = validate_invoice_preflight(&uniform);
        assert!(
            errs.is_empty(),
            "a uniform all-AAM invoice must be accepted; got {errs:?}"
        );
    }

    /// The three ADR-0101 kind variants' discriminant + field_path route to
    /// the per-line `vatRateKind` selector (the SPA renderer keys on the
    /// first two) and the bilingual messages name the offending kind.
    #[test]
    fn adr0101_kind_variants_discriminant_field_path_and_messages() {
        let nonzero = InvoicePreflightError::NonZeroPercentForExemptKind {
            line_index: 2,
            kind: aberp_billing::VatRateKind::AamExempt,
            actual: 27,
        };
        assert_eq!(nonzero.kind(), "NonZeroPercentForExemptKind");
        assert_eq!(nonzero.field_path(), "lines[2].vatRateKind");
        assert!(nonzero.message_hu().contains("AamExempt"));
        assert!(nonzero.message_en().contains("AamExempt"));

        let deferred = InvoicePreflightError::VatRateKindNotSupportedYet {
            line_index: 0,
            kind: aberp_billing::VatRateKind::TamExempt,
        };
        assert_eq!(deferred.kind(), "VatRateKindNotSupportedYet");
        assert_eq!(deferred.field_path(), "lines[0].vatRateKind");
        assert!(deferred.message_hu().contains("TamExempt"));
        assert!(deferred.message_en().contains("TamExempt"));

        let mixed = InvoicePreflightError::MixedVatRateKindsUnsupported {
            line_index: 1,
            first_kind: aberp_billing::VatRateKind::Percent,
            other_kind: aberp_billing::VatRateKind::AamExempt,
        };
        assert_eq!(mixed.kind(), "MixedVatRateKindsUnsupported");
        assert_eq!(mixed.field_path(), "lines[1].vatRateKind");
        assert!(mixed.message_en().contains("AamExempt"));
    }

    /// The kind branch did not swallow the existing numeric gate: a
    /// `Percent` line with an out-of-vocab rate still trips
    /// `LineItemVatRateUnknown` (unchanged behaviour).
    #[test]
    fn percent_kind_still_numeric_gated() {
        let mut r = good_request();
        r.lines[0].vat_rate_kind = aberp_billing::VatRateKind::Percent;
        r.lines[0].vat_rate_percent = 9; // not in {0,5,18,27}
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter()
                .any(|e| matches!(e, InvoicePreflightError::LineItemVatRateUnknown { .. })),
            "Percent + out-of-vocab rate must still trip LineItemVatRateUnknown; got {errs:?}"
        );
    }

    #[test]
    fn fires_customer_name_empty_for_blank_name() {
        let mut r = good_request();
        r.customer.name = "   ".to_string();
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::CustomerNameEmpty),
            "expected CustomerNameEmpty, got {errs:?}"
        );
    }

    #[test]
    fn fires_customer_tax_number_missing_for_blank_tax() {
        let mut r = good_request();
        r.customer.tax_number = "".to_string();
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::CustomerTaxNumberMissing),
            "expected CustomerTaxNumberMissing, got {errs:?}"
        );
    }

    #[test]
    fn fires_customer_tax_number_malformed_for_bare_digits() {
        let mut r = good_request();
        r.customer.tax_number = "87654321".to_string();
        let errs = validate_invoice_preflight(&r);
        let found = errs.iter().any(|e| {
            matches!(
                e,
                InvoicePreflightError::CustomerTaxNumberMalformed { actual, .. }
                    if actual == "87654321"
            )
        });
        assert!(found, "expected CustomerTaxNumberMalformed, got {errs:?}");
    }

    /// PR-77 / session-101 — `CustomerAddressMissing` fires when the
    /// customer.address is None entirely (the pre-PR-77 wire shape).
    /// This is the load-bearing arm — invoice 18 ABORTED because the
    /// pre-PR-77 SPA never sent an address; this gate must catch the
    /// same scenario locally now.
    #[test]
    fn fires_customer_address_missing_for_none_address() {
        let mut r = good_request();
        r.customer.address = None;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::CustomerAddressMissing),
            "expected CustomerAddressMissing for None address, got {errs:?}"
        );
    }

    /// PR-77 / session-101 — `CustomerAddressMissing` fires when ANY
    /// sub-field of an otherwise-present address is blank-after-trim.
    /// The four sub-fields are equally load-bearing on the NAV wire
    /// body (`<common:countryCode>`, `<common:postalCode>`,
    /// `<common:city>`, `<common:additionalAddressDetail>`); a blank
    /// in any of them is the same trap door as a missing block.
    #[test]
    fn fires_customer_address_missing_for_each_blank_subfield() {
        // city blank.
        let mut r = good_request();
        r.customer.address.as_mut().unwrap().city = "   ".to_string();
        assert!(
            validate_invoice_preflight(&r).contains(&InvoicePreflightError::CustomerAddressMissing),
            "blank city must fire CustomerAddressMissing"
        );

        // street blank.
        let mut r = good_request();
        r.customer.address.as_mut().unwrap().street = "".to_string();
        assert!(
            validate_invoice_preflight(&r).contains(&InvoicePreflightError::CustomerAddressMissing),
            "blank street must fire CustomerAddressMissing"
        );

        // postal_code blank.
        let mut r = good_request();
        r.customer.address.as_mut().unwrap().postal_code = "".to_string();
        assert!(
            validate_invoice_preflight(&r).contains(&InvoicePreflightError::CustomerAddressMissing),
            "blank postal_code must fire CustomerAddressMissing"
        );

        // country_code blank.
        let mut r = good_request();
        r.customer.address.as_mut().unwrap().country_code = "".to_string();
        assert!(
            validate_invoice_preflight(&r).contains(&InvoicePreflightError::CustomerAddressMissing),
            "blank country_code must fire CustomerAddressMissing"
        );
    }

    /// PR-77 / session-101 — the address gate is GATED on a well-formed
    /// tax number. A malformed tax number suppresses
    /// `CustomerAddressMissing` (the operator has a more proximate
    /// problem to fix first, and the address rule only applies in the
    /// business-buyer branch). This pin guards against accidental
    /// firing on the bare-digit-no-dash class of malformed entries.
    #[test]
    fn customer_address_missing_suppressed_when_tax_number_malformed() {
        let mut r = good_request();
        r.customer.tax_number = "12345".to_string(); // malformed
        r.customer.address = None;
        let errs = validate_invoice_preflight(&r);
        assert!(
            !errs.contains(&InvoicePreflightError::CustomerAddressMissing),
            "malformed tax must suppress CustomerAddressMissing, got {errs:?}"
        );
    }

    #[test]
    fn fires_invoice_lines_empty_for_empty_lines() {
        let mut r = good_request();
        r.lines.clear();
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::InvoiceLinesEmpty),
            "expected InvoiceLinesEmpty, got {errs:?}"
        );
    }

    #[test]
    fn fires_line_item_description_empty_for_blank_line_description() {
        let mut r = good_request();
        r.lines[0].description = "   ".to_string();
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::LineItemDescriptionEmpty { line_index: 0 }),
            "expected LineItemDescriptionEmpty, got {errs:?}"
        );
    }

    #[test]
    fn fires_line_item_quantity_zero_for_zero_quantity() {
        let mut r = good_request();
        r.lines[0].quantity = Decimal::ZERO;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::LineItemQuantityZero { line_index: 0 }),
            "expected LineItemQuantityZero, got {errs:?}"
        );
    }

    #[test]
    fn fires_line_item_quantity_zero_for_negative_quantity() {
        // S157 — negative is now reachable (Decimal, not u32) and must
        // collapse onto the same positive-quantity gate as zero.
        let mut r = good_request();
        r.lines[0].quantity = Decimal::from(-1);
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::LineItemQuantityZero { line_index: 0 }),
            "expected LineItemQuantityZero for negative, got {errs:?}"
        );
    }

    #[test]
    fn accepts_fractional_quantity() {
        // S157 — the headline behaviour: 1.5 days is a valid quantity.
        let mut r = good_request();
        r.lines[0].quantity = Decimal::new(15, 1); // 1.5
        let errs = validate_invoice_preflight(&r);
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, InvoicePreflightError::LineItemQuantityZero { .. })),
            "1.5 must not trip the quantity gate, got {errs:?}"
        );
    }

    #[test]
    fn fires_line_item_unit_price_non_positive_for_zero_unit_price() {
        let mut r = good_request();
        r.lines[0].unit_price = 0;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                InvoicePreflightError::LineItemUnitPriceNonPositive {
                    line_index: 0,
                    actual: 0
                }
            )),
            "expected LineItemUnitPriceNonPositive(0), got {errs:?}"
        );
    }

    #[test]
    fn fires_line_item_unit_price_non_positive_for_negative_unit_price() {
        let mut r = good_request();
        r.lines[0].unit_price = -500;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                InvoicePreflightError::LineItemUnitPriceNonPositive {
                    line_index: 0,
                    actual: -500
                }
            )),
            "expected LineItemUnitPriceNonPositive(-500), got {errs:?}"
        );
    }

    #[test]
    fn fires_line_item_vat_rate_unknown_for_off_vocab_rate() {
        let mut r = good_request();
        r.lines[0].vat_rate_percent = 12;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                InvoicePreflightError::LineItemVatRateUnknown {
                    line_index: 0,
                    actual: 12,
                    ..
                }
            )),
            "expected LineItemVatRateUnknown(12), got {errs:?}"
        );
    }

    #[test]
    fn allows_every_standard_vat_rate_individually() {
        for rate in ALLOWED_VAT_RATES_PERCENT {
            let mut r = good_request();
            r.lines[0].vat_rate_percent = *rate;
            let errs = validate_invoice_preflight(&r);
            assert!(
                errs.is_empty(),
                "rate {rate}% must pass preflight, got {errs:?}"
            );
        }
    }

    // ── multi-error pin ────────────────────────────────────────────────

    /// Worst-case pin: many errors at once must ALL surface in one
    /// pass (no early-bail). CLAUDE.md rule 9 — a regression that
    /// collapses the collector to a constant cannot pass this.
    #[test]
    fn collects_all_errors_in_one_pass_for_multi_failing_request() {
        let r = IssueInvoiceRequest {
            customer: CustomerJson {
                community_vat_number: None,
                vat_status: CustomerVatStatus::Domestic,
                partner_id: None,
                tax_number: "12345".to_string(), // malformed
                name: "  ".to_string(),          // blank
                address: None,                   // PR-77 — also missing
            },
            lines: vec![
                LineJson {
                    description: "".to_string(), // empty
                    quantity: Decimal::ZERO,     // zero
                    unit_price: -1,              // negative
                    vat_rate_percent: 12,        // off-vocab
                    vat_rate_kind: aberp_billing::VatRateKind::Percent,
                    note: None,
                    unit: None,
                },
                good_line(), // line 1 is fine
                LineJson {
                    description: "second bad line".to_string(),
                    quantity: Decimal::from(1),
                    unit_price: 10,
                    vat_rate_percent: 99, // off-vocab
                    vat_rate_kind: aberp_billing::VatRateKind::Percent,
                    note: None,
                    unit: None,
                },
            ],
            currency: Currency::Huf,
            series: None,
            bank_account_id: None,
            invoice_note: None,
            payment_deadline: None,
            delivery_date: None,
            delivery_date_override: None,
            email_buyer_on_issue: Some(false),
            submit_to_nav_on_issue: Some(false),
            // S160 — preflight fixtures use the default payment method.
            payment_method: aberp_billing::PaymentMethod::default(),
            email_recipient_override: None,
        };
        let errs = validate_invoice_preflight(&r);

        // Customer × 2
        assert!(errs.contains(&InvoicePreflightError::CustomerNameEmpty));
        assert!(errs
            .iter()
            .any(|e| matches!(e, InvoicePreflightError::CustomerTaxNumberMalformed { .. })));

        // Line 0 × 4
        assert!(errs.contains(&InvoicePreflightError::LineItemDescriptionEmpty { line_index: 0 }));
        assert!(errs.contains(&InvoicePreflightError::LineItemQuantityZero { line_index: 0 }));
        assert!(errs.iter().any(|e| matches!(
            e,
            InvoicePreflightError::LineItemUnitPriceNonPositive { line_index: 0, .. }
        )));
        assert!(errs.iter().any(|e| matches!(
            e,
            InvoicePreflightError::LineItemVatRateUnknown {
                line_index: 0,
                actual: 12,
                ..
            }
        )));

        // Line 2 × 1
        assert!(errs.iter().any(|e| matches!(
            e,
            InvoicePreflightError::LineItemVatRateUnknown {
                line_index: 2,
                actual: 99,
                ..
            }
        )));

        // Line 1 is good → no errors with line_index 1.
        assert!(!errs.iter().any(|e| match e {
            InvoicePreflightError::LineItemDescriptionEmpty { line_index }
            | InvoicePreflightError::LineItemQuantityZero { line_index }
            | InvoicePreflightError::LineItemUnitPriceNonPositive { line_index, .. }
            | InvoicePreflightError::LineItemVatRateUnknown { line_index, .. } => *line_index == 1,
            _ => false,
        }));
    }

    // ── cross-source pin (preflight ⊆ NAV) ─────────────────────────────

    /// A malformed customer ADÓSZÁM would fail at NAV submit time
    /// (server-side schema validation catches the structured-tax
    /// children if the customer is a Hungarian taxpayer). Preflight
    /// catches it BEFORE issuance so the operator fixes inline. Pin
    /// that the two surfaces agree on the malformed-input case.
    #[test]
    fn preflight_catches_what_would_fail_at_nav_submit_time() {
        let mut r = good_request();
        r.customer.tax_number = "87654321-2-".to_string(); // missing county code

        // 1. Preflight catches it.
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                InvoicePreflightError::CustomerTaxNumberMalformed { actual, .. }
                    if actual == "87654321-2-"
            )),
            "preflight must catch malformed customer ADÓSZÁM, got {errs:?}"
        );

        // 2. The same parser NAV-side (`parse_hungarian_tax_number`) also
        //    rejects it — proves preflight is strictly stronger than
        //    nothing and weaker than NAV (we don't claim to mirror NAV
        //    exactly per ADR-0038 §"Adversarial review").
        let nav_side = parse_hungarian_tax_number("87654321-2-");
        assert!(
            nav_side.is_err(),
            "supplier-side parser must also reject the same input — preflight ⊆ NAV invariant"
        );
    }

    // ── accessor coverage ──────────────────────────────────────────────

    #[test]
    fn kind_is_distinct_per_variant() {
        // CLAUDE.md rule 9 — a regression that collapses two variants'
        // `kind()` outputs to the same string fails loud here.
        let variants = [
            InvoicePreflightError::CustomerNameEmpty,
            InvoicePreflightError::CustomerTaxNumberMissing,
            InvoicePreflightError::CustomerTaxNumberMalformed {
                actual: "x".to_string(),
                reason: "r",
            },
            InvoicePreflightError::InvoiceLinesEmpty,
            InvoicePreflightError::LineItemDescriptionEmpty { line_index: 0 },
            InvoicePreflightError::LineItemQuantityZero { line_index: 0 },
            InvoicePreflightError::LineItemUnitPriceNonPositive {
                line_index: 0,
                actual: 0,
            },
            InvoicePreflightError::LineItemVatRateUnknown {
                line_index: 0,
                actual: 0,
                allowed: ALLOWED_VAT_RATES_PERCENT,
            },
            // PR-73 / ADR-0040 §addendum.
            InvoicePreflightError::SellerBankMissingForCurrency {
                currency: Currency::Eur,
            },
            InvoicePreflightError::SellerBankCurrencyMismatch {
                selected_id: "bnk_x".to_string(),
                selected_currency: Currency::Huf,
                invoice_currency: Currency::Eur,
            },
            // PR-97 / ADR-0048.
            InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson {
                actual: "12345678-1-42".to_string(),
            },
            // ADR-0102 — EU-partner customer type + cross-field matrix.
            InvoicePreflightError::CustomerTaxNumberPresentForOther {
                actual: "12345678-1-42".to_string(),
            },
            InvoicePreflightError::CommunityVatNumberMissing,
            InvoicePreflightError::CommunityVatNumberMalformed {
                actual: "ZZ1".to_string(),
                reason: "bad".to_string(),
            },
            // NAV-adversarial FIX #1.
            InvoicePreflightError::CustomerCountryCodeInvalid {
                actual: "Austria".to_string(),
                reason: "not alpha-2".to_string(),
            },
            InvoicePreflightError::VatKindRequiresOtherBuyer {
                line_index: 0,
                kind: VatRateKind::IntraCommunityGoods,
                actual_status: CustomerVatStatus::Domestic,
            },
            // FIX #2 — DomesticReverseCharge is the only Domestic-required
            // kind now (AAM is buyer-agnostic).
            InvoicePreflightError::VatKindRequiresDomesticBuyer {
                line_index: 0,
                kind: VatRateKind::DomesticReverseCharge,
                actual_status: CustomerVatStatus::Other,
            },
        ];
        let kinds: std::collections::HashSet<&'static str> =
            variants.iter().map(|v| v.kind()).collect();
        assert_eq!(
            kinds.len(),
            variants.len(),
            "every variant must have a distinct kind() discriminant; got duplicates: {kinds:?}"
        );
    }

    #[test]
    fn field_path_uses_camel_case_and_bracket_indexing() {
        assert_eq!(
            InvoicePreflightError::CustomerNameEmpty.field_path(),
            "customer.name"
        );
        assert_eq!(
            InvoicePreflightError::CustomerTaxNumberMissing.field_path(),
            "customer.taxNumber"
        );
        assert_eq!(
            InvoicePreflightError::LineItemDescriptionEmpty { line_index: 3 }.field_path(),
            "lines[3].description"
        );
        assert_eq!(
            InvoicePreflightError::LineItemVatRateUnknown {
                line_index: 7,
                actual: 99,
                allowed: ALLOWED_VAT_RATES_PERCENT,
            }
            .field_path(),
            "lines[7].vatRatePercent"
        );
    }

    #[test]
    fn message_hu_and_en_both_present_for_every_variant() {
        // Every variant must have non-empty HU + EN messages so the
        // SPA's inline renderer never falls through to "(no message)".
        let variants = [
            InvoicePreflightError::CustomerNameEmpty,
            InvoicePreflightError::CustomerTaxNumberMissing,
            InvoicePreflightError::CustomerTaxNumberMalformed {
                actual: "1234".to_string(),
                reason: "expected three dash-separated segments",
            },
            InvoicePreflightError::InvoiceLinesEmpty,
            InvoicePreflightError::LineItemDescriptionEmpty { line_index: 0 },
            InvoicePreflightError::LineItemQuantityZero { line_index: 0 },
            InvoicePreflightError::LineItemUnitPriceNonPositive {
                line_index: 0,
                actual: -1,
            },
            InvoicePreflightError::LineItemVatRateUnknown {
                line_index: 0,
                actual: 99,
                allowed: ALLOWED_VAT_RATES_PERCENT,
            },
            // PR-73 / ADR-0040 §addendum.
            InvoicePreflightError::SellerBankMissingForCurrency {
                currency: Currency::Eur,
            },
            InvoicePreflightError::SellerBankCurrencyMismatch {
                selected_id: "bnk_x".to_string(),
                selected_currency: Currency::Huf,
                invoice_currency: Currency::Eur,
            },
            // PR-97 / ADR-0048.
            InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson {
                actual: "12345678-1-42".to_string(),
            },
            // ADR-0102 — EU-partner customer type + cross-field matrix.
            InvoicePreflightError::CustomerTaxNumberPresentForOther {
                actual: "12345678-1-42".to_string(),
            },
            InvoicePreflightError::CommunityVatNumberMissing,
            InvoicePreflightError::CommunityVatNumberMalformed {
                actual: "ZZ1".to_string(),
                reason: "bad".to_string(),
            },
            // NAV-adversarial FIX #1.
            InvoicePreflightError::CustomerCountryCodeInvalid {
                actual: "Austria".to_string(),
                reason: "not alpha-2".to_string(),
            },
            InvoicePreflightError::VatKindRequiresOtherBuyer {
                line_index: 0,
                kind: VatRateKind::IntraCommunityGoods,
                actual_status: CustomerVatStatus::Domestic,
            },
            InvoicePreflightError::VatKindRequiresDomesticBuyer {
                line_index: 0,
                kind: VatRateKind::DomesticReverseCharge,
                actual_status: CustomerVatStatus::Other,
            },
        ];
        for v in variants {
            assert!(!v.message_hu().is_empty(), "HU missing for {v:?}");
            assert!(!v.message_en().is_empty(), "EN missing for {v:?}");
        }
    }

    /// PR-73 / ADR-0040 §addendum — field-path closed-vocab: both new
    /// bank-related variants route to the `bankAccountId` form input.
    /// Pinned so the SPA's inline-error renderer has a stable target;
    /// a regression that drifts the path (e.g. `bank.id`) would defeat
    /// the per-field highlight.
    #[test]
    fn seller_bank_variants_route_to_bank_account_id_field() {
        let missing = InvoicePreflightError::SellerBankMissingForCurrency {
            currency: Currency::Huf,
        };
        assert_eq!(missing.field_path(), "bankAccountId");

        let mismatch = InvoicePreflightError::SellerBankCurrencyMismatch {
            selected_id: "bnk_abc".to_string(),
            selected_currency: Currency::Eur,
            invoice_currency: Currency::Huf,
        };
        assert_eq!(mismatch.field_path(), "bankAccountId");
    }

    /// PR-73 / ADR-0040 §addendum — `SellerBankMissingForCurrency`
    /// bilingual messages name the rejected currency. Pinned so the
    /// operator-facing copy never silently drops the EUR-vs-HUF
    /// detail (the only operator-actionable handle).
    #[test]
    fn seller_bank_missing_for_currency_message_names_currency() {
        let err = InvoicePreflightError::SellerBankMissingForCurrency {
            currency: Currency::Eur,
        };
        let hu = err.message_hu();
        let en = err.message_en();
        assert!(hu.contains("EUR"), "Hungarian must name currency: {hu}");
        assert!(en.contains("EUR"), "English must name currency: {en}");
        // Hint pointer to Tenant Settings — operator-actionable affordance.
        assert!(
            hu.contains("Bérlőbeállítások") || hu.contains("Bank accounts"),
            "Hungarian must hint at Tenant Settings: {hu}"
        );
        assert!(
            en.contains("Tenant Settings"),
            "English must hint at Tenant Settings: {en}"
        );
    }

    /// PR-73 / ADR-0040 §addendum — `SellerBankCurrencyMismatch`
    /// bilingual messages name BOTH currencies AND the selected id.
    #[test]
    fn seller_bank_currency_mismatch_message_names_all_three_handles() {
        let err = InvoicePreflightError::SellerBankCurrencyMismatch {
            selected_id: "bnk_xyz".to_string(),
            selected_currency: Currency::Huf,
            invoice_currency: Currency::Eur,
        };
        let hu = err.message_hu();
        let en = err.message_en();
        for body in [&hu, &en] {
            assert!(
                body.contains("bnk_xyz"),
                "message must name selected_id: {body}"
            );
            assert!(
                body.contains("HUF"),
                "message must name selected currency: {body}"
            );
            assert!(
                body.contains("EUR"),
                "message must name invoice currency: {body}"
            );
        }
    }

    // ── PR-97 / ADR-0048 §6 — conditional rules per buyer kind ────────

    /// PrivatePerson buyer with a populated `tax_number` fires the
    /// symmetric `CustomerTaxNumberPresentForPrivatePerson` variant. The
    /// NAV business-rule layer rejects this combination; surfacing it at
    /// preflight prevents the sequence-burn that would otherwise land at
    /// submit time.
    #[test]
    fn fires_customer_tax_number_present_for_private_person() {
        let mut r = good_request();
        r.customer.vat_status = CustomerVatStatus::PrivatePerson;
        r.customer.tax_number = "12345678-1-42".to_string();
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson { actual }
                    if actual == "12345678-1-42"
            )),
            "expected CustomerTaxNumberPresentForPrivatePerson, got {errs:?}"
        );
    }

    /// PrivatePerson buyer with NO `tax_number` must NOT fire
    /// `CustomerTaxNumberMissing` — that gate is now gated on
    /// `vat_status == Domestic`. (The §169 address gate DOES fire for
    /// PrivatePerson now — session-150 — so this test keeps a full
    /// address on the request to isolate the tax-number signal.)
    #[test]
    fn does_not_fire_customer_tax_number_missing_for_private_person() {
        let mut r = good_request();
        r.customer.vat_status = CustomerVatStatus::PrivatePerson;
        r.customer.tax_number = "".to_string();
        let errs = validate_invoice_preflight(&r);
        assert!(
            !errs.contains(&InvoicePreflightError::CustomerTaxNumberMissing),
            "PrivatePerson + empty tax must NOT fire CustomerTaxNumberMissing, got {errs:?}"
        );
    }

    /// Session-150 — PrivatePerson buyer with NO address now FIRES
    /// `CustomerAddressMissing`. Áfa tv. §169 mandates the buyer address
    /// on the printed/PDF invoice for ALL customer types; the PR-97 /
    /// ADR-0048 override-2 carve-out (PrivatePerson tolerates
    /// address-absent) is reverted on the same legal foundation as the
    /// session-148 buyer-name rule. (Inverts the former
    /// `does_not_fire_customer_address_missing_for_private_person`.)
    #[test]
    fn fires_customer_address_missing_for_private_person() {
        let mut r = good_request();
        r.customer.vat_status = CustomerVatStatus::PrivatePerson;
        r.customer.tax_number = "".to_string();
        r.customer.address = None;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::CustomerAddressMissing),
            "PrivatePerson + None address must fire CustomerAddressMissing (§169), got {errs:?}"
        );
    }

    /// Session-150 — the §169 buyer-address rule fires for EVERY
    /// customer type when the address is absent. Mirrors
    /// `fires_customer_name_empty_for_every_customer_type`. (Other also
    /// surfaces its v1-deferral error, but the address gate fires
    /// regardless for Domestic and PrivatePerson; Other short-circuits
    /// before the address check, so it is exercised in
    /// `fires_customer_vat_status_other_not_supported_v1` instead.)
    #[test]
    fn fires_customer_address_missing_for_domestic_and_private_person() {
        for (vat_status, tax) in [
            (CustomerVatStatus::Domestic, "12345678-2-13"),
            (CustomerVatStatus::PrivatePerson, ""),
        ] {
            let mut r = good_request();
            r.customer.vat_status = vat_status;
            r.customer.tax_number = tax.to_string();
            r.customer.address = None;
            let errs = validate_invoice_preflight(&r);
            assert!(
                errs.contains(&InvoicePreflightError::CustomerAddressMissing),
                "{vat_status:?} + None address must fire CustomerAddressMissing (§169), got {errs:?}"
            );
        }
    }

    /// Session-150 — happy path: a buyer with a complete address issues
    /// cleanly (no `CustomerAddressMissing`) for every supported
    /// customer type. Pins that the §169 gate does not over-fire.
    #[test]
    fn does_not_fire_customer_address_missing_when_address_present() {
        for (vat_status, tax) in [
            (CustomerVatStatus::Domestic, "12345678-2-13"),
            (CustomerVatStatus::PrivatePerson, ""),
        ] {
            let mut r = good_request();
            r.customer.vat_status = vat_status;
            r.customer.tax_number = tax.to_string();
            r.customer.address = Some(good_customer_address());
            let errs = validate_invoice_preflight(&r);
            assert!(
                !errs.contains(&InvoicePreflightError::CustomerAddressMissing),
                "{vat_status:?} + complete address must NOT fire CustomerAddressMissing, got {errs:?}"
            );
        }
    }

    /// Session-150 — the §169 address message names the statute in both
    /// languages so the SPA's inline chip is operator-actionable.
    /// Mirrors `customer_name_empty_message_names_section_169`.
    #[test]
    fn customer_address_missing_message_names_section_169() {
        let e = InvoicePreflightError::CustomerAddressMissing;
        assert!(
            e.message_hu().contains("§169"),
            "HU message must cite §169, got {}",
            e.message_hu()
        );
        assert!(
            e.message_en().contains("§169"),
            "EN message must cite §169, got {}",
            e.message_en()
        );
    }

    /// ADR-0102 — an `Other` buyer with a stray HU tax number and no EU
    /// VAT number surfaces BOTH the tax-number-forbidden and the
    /// community-VAT-missing errors (the two `Other`-completeness rules).
    /// `good_request()` carries a Domestic tax number + no community VAT,
    /// so flipping to Other trips both.
    #[test]
    fn other_buyer_without_eu_vat_and_with_hu_tax_fires_both_rules() {
        let mut r = good_request();
        r.customer.vat_status = CustomerVatStatus::Other;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter()
                .any(|e| matches!(e, InvoicePreflightError::CommunityVatNumberMissing)),
            "expected CommunityVatNumberMissing, got {errs:?}"
        );
        assert!(
            errs.iter().any(|e| matches!(
                e,
                InvoicePreflightError::CustomerTaxNumberPresentForOther { .. }
            )),
            "expected CustomerTaxNumberPresentForOther, got {errs:?}"
        );
    }

    /// ADR-0102 — a fully-formed `Other` buyer (EU VAT number present +
    /// valid, no HU tax number, address complete) passes preflight on a
    /// plain `Percent` invoice (Percent is buyer-agnostic).
    #[test]
    fn well_formed_other_buyer_with_percent_line_passes() {
        let mut r = good_request();
        r.customer.vat_status = CustomerVatStatus::Other;
        r.customer.tax_number = "".to_string();
        r.customer.community_vat_number = Some("ATU12345678".to_string());
        let errs = validate_invoice_preflight(&r);
        assert!(errs.is_empty(), "expected no errors, got {errs:?}");
    }

    /// ADR-0102 — helper: a well-formed `Other` (EU) buyer.
    fn make_other_buyer(r: &mut IssueInvoiceRequest) {
        r.customer.vat_status = CustomerVatStatus::Other;
        r.customer.tax_number = "".to_string();
        r.customer.community_vat_number = Some("ATU12345678".to_string());
    }

    /// ADR-0102 §4(a) — THE CROSS-FIELD MATRIX. One row per compliance
    /// case; the dangerous direction (EU-0 line + domestic buyer) MUST
    /// reject, and the correct EU-0 assembly (EU-0 line + Other buyer +
    /// EU VAT) MUST accept.
    #[test]
    fn cross_field_matrix_eu0_requires_other_buyer() {
        use aberp_billing::VatRateKind::*;

        // EU-0 goods + Domestic buyer → reject VatKindRequiresOtherBuyer.
        for kind in [IntraCommunityGoods, IntraCommunityServiceReverse] {
            let mut r = good_request();
            r.lines[0].vat_rate_kind = kind;
            r.lines[0].vat_rate_percent = 0;
            // buyer stays Domestic (good_request default)
            let errs = validate_invoice_preflight(&r);
            assert!(
                errs.iter().any(|e| matches!(
                    e,
                    InvoicePreflightError::VatKindRequiresOtherBuyer { kind: k, .. } if *k == kind
                )),
                "{kind:?} + Domestic must fire VatKindRequiresOtherBuyer; got {errs:?}"
            );
        }

        // EU-0 goods + Other buyer w/ EU VAT → accept.
        for kind in [IntraCommunityGoods, IntraCommunityServiceReverse] {
            let mut r = good_request();
            make_other_buyer(&mut r);
            r.lines[0].vat_rate_kind = kind;
            r.lines[0].vat_rate_percent = 0;
            let errs = validate_invoice_preflight(&r);
            assert!(
                errs.is_empty(),
                "{kind:?} + well-formed Other buyer must accept; got {errs:?}"
            );
        }
    }

    /// ADR-0102 §4(a) — `DomesticReverseCharge` REQUIRES a Domestic buyer
    /// (§142 belföldi = two domestic taxable persons); pairing it with an
    /// `Other` buyer rejects. DRC + Domestic accepts (happy path).
    #[test]
    fn cross_field_matrix_domestic_reverse_charge_requires_domestic_buyer() {
        use aberp_billing::VatRateKind::*;

        // DRC + Other buyer → reject VatKindRequiresDomesticBuyer.
        let mut r = good_request();
        make_other_buyer(&mut r);
        r.lines[0].vat_rate_kind = DomesticReverseCharge;
        r.lines[0].vat_rate_percent = 0;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                InvoicePreflightError::VatKindRequiresDomesticBuyer {
                    kind: DomesticReverseCharge,
                    ..
                }
            )),
            "DRC + Other must fire VatKindRequiresDomesticBuyer; got {errs:?}"
        );

        // DRC + Domestic buyer → accept (the happy path stays green).
        let mut r = good_request();
        r.lines[0].vat_rate_kind = DomesticReverseCharge;
        r.lines[0].vat_rate_percent = 0;
        let errs = validate_invoice_preflight(&r);
        assert!(errs.is_empty(), "DRC + Domestic must accept; got {errs:?}");
    }

    /// NAV-adversarial FIX #2 (2026-07-16) — `AamExempt` (AAM, alanyi
    /// adómentesség §187) is a SELLER-side exemption: NAV binds NO
    /// customerVatStatus to it, so AAM must be BUYER-AGNOSTIC (like
    /// `Percent`). The previous strict "AAM requires Domestic" wrongly
    /// blocked the common exempt-small-business cases. This pins that AAM
    /// now ACCEPTS against Domestic, PrivatePerson, AND Other buyers, and
    /// NEVER fires `VatKindRequiresDomesticBuyer`.
    #[test]
    fn aam_is_buyer_agnostic() {
        use aberp_billing::VatRateKind::*;

        // AAM + Domestic → accept.
        let mut r = good_request();
        r.lines[0].vat_rate_kind = AamExempt;
        r.lines[0].vat_rate_percent = 0;
        let errs = validate_invoice_preflight(&r);
        assert!(errs.is_empty(), "AAM + Domestic must accept; got {errs:?}");

        // AAM + PrivatePerson → accept (the headline legitimate case:
        // exempt small business invoicing a magánszemély). PrivatePerson
        // forbids a tax number, so clear it.
        let mut r = good_request();
        r.customer.vat_status = CustomerVatStatus::PrivatePerson;
        r.customer.tax_number = "".to_string();
        r.lines[0].vat_rate_kind = AamExempt;
        r.lines[0].vat_rate_percent = 0;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.is_empty(),
            "AAM + PrivatePerson must accept (FIX #2); got {errs:?}"
        );

        // AAM + Other (well-formed EU buyer) → accept.
        let mut r = good_request();
        make_other_buyer(&mut r);
        r.lines[0].vat_rate_kind = AamExempt;
        r.lines[0].vat_rate_percent = 0;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.is_empty(),
            "AAM + Other must accept (FIX #2); got {errs:?}"
        );

        // Across every buyer status, AAM must NEVER fire the domestic-buyer
        // requirement (the exact bug FIX #2 removes).
        for status in [
            CustomerVatStatus::Domestic,
            CustomerVatStatus::PrivatePerson,
            CustomerVatStatus::Other,
        ] {
            let mut r = good_request();
            match status {
                CustomerVatStatus::Other => make_other_buyer(&mut r),
                CustomerVatStatus::PrivatePerson => {
                    r.customer.vat_status = status;
                    r.customer.tax_number = "".to_string();
                }
                CustomerVatStatus::Domestic => {}
            }
            r.lines[0].vat_rate_kind = AamExempt;
            r.lines[0].vat_rate_percent = 0;
            let errs = validate_invoice_preflight(&r);
            assert!(
                !errs.iter().any(|e| matches!(
                    e,
                    InvoicePreflightError::VatKindRequiresDomesticBuyer { .. }
                )),
                "AAM must never fire VatKindRequiresDomesticBuyer ({status:?}); got {errs:?}"
            );
        }
    }

    /// ADR-0102 §4(b) — an `Other` buyer with a malformed EU VAT number
    /// fires `CommunityVatNumberMalformed` (structural VIES shape).
    #[test]
    fn other_buyer_with_malformed_eu_vat_rejects() {
        let mut r = good_request();
        r.customer.vat_status = CustomerVatStatus::Other;
        r.customer.tax_number = "".to_string();
        r.customer.community_vat_number = Some("ZZ99".to_string());
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.iter()
                .any(|e| matches!(e, InvoicePreflightError::CommunityVatNumberMalformed { .. })),
            "malformed EU VAT must fire CommunityVatNumberMalformed; got {errs:?}"
        );
    }

    /// NAV-adversarial FIX #1 — an `Other` (foreign-EU) buyer whose
    /// address `country_code` is a full country NAME (the "Austria" /
    /// "Ausztria" partner-record case) fires `CustomerCountryCodeInvalid`
    /// at preflight rather than bouncing at NAV. A well-formed alpha-2
    /// code (`AT`) does NOT fire it.
    #[test]
    fn other_buyer_with_country_name_fires_country_code_invalid() {
        // Country NAME → reject.
        for bad in ["Austria", "Ausztria", "at"] {
            let mut r = good_request();
            make_other_buyer(&mut r);
            r.lines[0].vat_rate_kind = aberp_billing::VatRateKind::IntraCommunityGoods;
            r.lines[0].vat_rate_percent = 0;
            r.customer.address.as_mut().unwrap().country_code = bad.to_string();
            let errs = validate_invoice_preflight(&r);
            assert!(
                errs.iter().any(|e| matches!(
                    e,
                    InvoicePreflightError::CustomerCountryCodeInvalid { actual, .. } if actual == bad
                )),
                "Other buyer with country `{bad}` must fire CustomerCountryCodeInvalid; got {errs:?}"
            );
        }

        // Valid alpha-2 → no country-code error (the happy EU-0 path).
        let mut r = good_request();
        make_other_buyer(&mut r);
        r.lines[0].vat_rate_kind = aberp_billing::VatRateKind::IntraCommunityGoods;
        r.lines[0].vat_rate_percent = 0;
        r.customer.address.as_mut().unwrap().country_code = "AT".to_string();
        let errs = validate_invoice_preflight(&r);
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, InvoicePreflightError::CustomerCountryCodeInvalid { .. })),
            "Other buyer with country `AT` must NOT fire CustomerCountryCodeInvalid; got {errs:?}"
        );
    }

    /// NAV-adversarial FIX #1 — the country-code guard is scoped to
    /// `Other` buyers ONLY (backward-compat): a Domestic buyer with a
    /// non-alpha-2 country in the address does NOT fire
    /// `CustomerCountryCodeInvalid` (Domestic buyers are HU-forced by the
    /// SPA; guarding them would be an out-of-scope behaviour change).
    #[test]
    fn domestic_buyer_country_code_not_guarded() {
        let mut r = good_request();
        // Domestic (good_request default) with a full country name.
        r.customer.address.as_mut().unwrap().country_code = "Hungary".to_string();
        let errs = validate_invoice_preflight(&r);
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, InvoicePreflightError::CustomerCountryCodeInvalid { .. })),
            "Domestic buyer must NOT be country-code-guarded (FIX #1 is Other-only); got {errs:?}"
        );
    }

    /// NAV-adversarial FIX #1 — the country-code variant routes to the
    /// address country input and its bilingual messages name the offending
    /// value.
    #[test]
    fn country_code_invalid_field_path_and_messages() {
        let e = InvoicePreflightError::CustomerCountryCodeInvalid {
            actual: "Austria".to_string(),
            reason: "not alpha-2".to_string(),
        };
        assert_eq!(e.kind(), "CustomerCountryCodeInvalid");
        assert_eq!(e.field_path(), "customer.address.countryCode");
        assert!(e.message_hu().contains("Austria"));
        assert!(e.message_en().contains("Austria"));
    }

    /// Domestic buyer's pre-PR-97 invariants still hold — sanity pin
    /// that the conditional switch did not regress the Domestic
    /// branch. A blank tax number still fires
    /// `CustomerTaxNumberMissing`; a present-but-incomplete address
    /// still fires `CustomerAddressMissing` (PR-77 hold).
    #[test]
    fn domestic_branch_preserves_pre_pr_97_invariants() {
        let mut r = good_request();
        r.customer.vat_status = CustomerVatStatus::Domestic;
        r.customer.tax_number = "".to_string();
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::CustomerTaxNumberMissing),
            "Domestic + empty tax must still fire CustomerTaxNumberMissing, got {errs:?}"
        );
    }

    /// Session-148 (Ervin override 3) — the §169 buyer-name rule is now
    /// UNCONDITIONAL. A whitespace-only `customer.name` must fire
    /// `CustomerNameEmpty` for EVERY customer type, including the
    /// PRIVATE_PERSON branch that the PR-97 GDPR carve-out used to
    /// exempt. (Other also surfaces its v1-deferral error, but the
    /// name gate fires regardless.)
    #[test]
    fn fires_customer_name_empty_for_every_customer_type() {
        for vat_status in [
            CustomerVatStatus::Domestic,
            CustomerVatStatus::PrivatePerson,
            CustomerVatStatus::Other,
        ] {
            let mut r = good_request();
            r.customer.vat_status = vat_status;
            // PrivatePerson tolerates no tax number; clear it so the
            // only name-related signal under test is CustomerNameEmpty.
            r.customer.tax_number = "".to_string();
            r.customer.name = "   ".to_string();
            let errs = validate_invoice_preflight(&r);
            assert!(
                errs.contains(&InvoicePreflightError::CustomerNameEmpty),
                "{vat_status:?} + blank name must fire CustomerNameEmpty (§169 unconditional), got {errs:?}"
            );
        }
    }

    /// Session-148 — the §169 message names the statute in both
    /// languages so the SPA's inline chip is operator-actionable.
    #[test]
    fn customer_name_empty_message_names_section_169() {
        let e = InvoicePreflightError::CustomerNameEmpty;
        assert!(
            e.message_hu().contains("§169"),
            "HU message must cite §169, got {}",
            e.message_hu()
        );
        assert!(
            e.message_en().contains("§169"),
            "EN message must cite §169, got {}",
            e.message_en()
        );
    }

    /// Session-148 — happy path: a buyer with a name present issues
    /// cleanly (no CustomerNameEmpty) for every supported customer
    /// type. Pins that the unconditional gate did not over-fire.
    #[test]
    fn does_not_fire_customer_name_empty_when_name_present() {
        for (vat_status, tax) in [
            (CustomerVatStatus::Domestic, "12345678-2-13"),
            (CustomerVatStatus::PrivatePerson, ""),
        ] {
            let mut r = good_request();
            r.customer.vat_status = vat_status;
            r.customer.tax_number = tax.to_string();
            r.customer.name = "Teszt Magánszemély".to_string();
            let errs = validate_invoice_preflight(&r);
            assert!(
                !errs.contains(&InvoicePreflightError::CustomerNameEmpty),
                "{vat_status:?} + present name must NOT fire CustomerNameEmpty, got {errs:?}"
            );
        }
    }

    /// Field-path closed-vocab: the new PR-97 variants route to the
    /// expected SPA form inputs so the inline-error renderer can target
    /// them.
    #[test]
    fn pr_97_variants_route_to_expected_fields() {
        let private_person_with_tax =
            InvoicePreflightError::CustomerTaxNumberPresentForPrivatePerson {
                actual: "x".to_string(),
            };
        assert_eq!(private_person_with_tax.field_path(), "customer.taxNumber");

        // ADR-0102 — the community-VAT variants route to the EU-VAT
        // input; the cross-field kind↔buyer mismatches route to the
        // buyer-type control (the fix is on the buyer side).
        let missing_eu_vat = InvoicePreflightError::CommunityVatNumberMissing;
        assert_eq!(missing_eu_vat.field_path(), "customer.communityVatNumber");

        let malformed_eu_vat = InvoicePreflightError::CommunityVatNumberMalformed {
            actual: "ZZ1".to_string(),
            reason: "bad".to_string(),
        };
        assert_eq!(malformed_eu_vat.field_path(), "customer.communityVatNumber");

        let eu_tax_present = InvoicePreflightError::CustomerTaxNumberPresentForOther {
            actual: "x".to_string(),
        };
        assert_eq!(eu_tax_present.field_path(), "customer.taxNumber");

        let needs_other = InvoicePreflightError::VatKindRequiresOtherBuyer {
            line_index: 0,
            kind: VatRateKind::IntraCommunityGoods,
            actual_status: CustomerVatStatus::Domestic,
        };
        assert_eq!(needs_other.field_path(), "customer.vatStatus");
    }
}
