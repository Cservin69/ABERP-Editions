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

use aberp_billing::Currency;

use crate::nav_xml::{parse_hungarian_tax_number, SupplierConfigError};
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
    /// `lines.is_empty()`. Lifted from the legacy
    /// `validate_issue_request` surface into the closed-vocab so the
    /// SPA's per-field renderer can target `lines` rather than
    /// rendering a string blob.
    InvoiceLinesEmpty,
    /// `lines[line_index].description.trim().is_empty()`.
    LineItemDescriptionEmpty { line_index: usize },
    /// `lines[line_index].quantity == 0`. u32 cannot be negative;
    /// non-positive collapses to zero on this wire shape.
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
            InvoicePreflightError::InvoiceLinesEmpty => "InvoiceLinesEmpty",
            InvoicePreflightError::LineItemDescriptionEmpty { .. } => "LineItemDescriptionEmpty",
            InvoicePreflightError::LineItemQuantityZero { .. } => "LineItemQuantityZero",
            InvoicePreflightError::LineItemUnitPriceNonPositive { .. } => {
                "LineItemUnitPriceNonPositive"
            }
            InvoicePreflightError::LineItemVatRateUnknown { .. } => "LineItemVatRateUnknown",
            InvoicePreflightError::SellerBankMissingForCurrency { .. } => {
                "SellerBankMissingForCurrency"
            }
            InvoicePreflightError::SellerBankCurrencyMismatch { .. } => {
                "SellerBankCurrencyMismatch"
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
            InvoicePreflightError::SellerBankMissingForCurrency { .. }
            | InvoicePreflightError::SellerBankCurrencyMismatch { .. } => {
                "bankAccountId".to_string()
            }
        }
    }

    /// Hungarian operator-facing message. Surfaced verbatim by the
    /// SPA's inline-error renderer.
    pub fn message_hu(&self) -> String {
        match self {
            InvoicePreflightError::CustomerNameEmpty => {
                "Az ügyfél neve kötelező.".to_string()
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
                format!(
                    "A(z) {}. tételsor mennyisége legalább 1 kell legyen.",
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
                    "A(z) {}. tételsor ÁFA-kulcsa ({actual}%) nem szerepel a magyar szabványos kulcsok között ({}). Speciális kategóriák (AAM/TAM/TAH) jelenleg nem támogatottak.",
                    line_index + 1,
                    format_percent_list(allowed)
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
        }
    }

    /// English developer / debug message. Surfaced verbatim by the
    /// SPA's inline-error renderer alongside the Hungarian message.
    pub fn message_en(&self) -> String {
        match self {
            InvoicePreflightError::CustomerNameEmpty => "Customer name is required.".to_string(),
            InvoicePreflightError::CustomerTaxNumberMissing => {
                "Customer ADÓSZÁM is required (expected `xxxxxxxx-y-zz`, e.g. `87654321-2-13`)."
                    .to_string()
            }
            InvoicePreflightError::CustomerTaxNumberMalformed { actual, reason } => {
                format!(
                    "Customer ADÓSZÁM `{actual}` is not a valid Hungarian tax number ({reason}); expected `xxxxxxxx-y-zz`, e.g. `87654321-2-13`."
                )
            }
            InvoicePreflightError::InvoiceLinesEmpty => {
                "At least one line item is required.".to_string()
            }
            InvoicePreflightError::LineItemDescriptionEmpty { line_index } => {
                format!("Line {} description is required.", line_index + 1)
            }
            InvoicePreflightError::LineItemQuantityZero { line_index } => {
                format!("Line {} quantity must be at least 1.", line_index + 1)
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
                    "Line {} VAT rate ({actual}%) is not a Hungarian standard rate (allowed: {}). Special categories (AAM/TAM/TAH) are not supported on this wire shape today.",
                    line_index + 1,
                    format_percent_list(allowed)
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
    if request.customer.name.trim().is_empty() {
        errors.push(InvoicePreflightError::CustomerNameEmpty);
    }
    let tax_trimmed = request.customer.tax_number.trim();
    if tax_trimmed.is_empty() {
        errors.push(InvoicePreflightError::CustomerTaxNumberMissing);
    } else {
        match parse_hungarian_tax_number(tax_trimmed) {
            Ok(_) => {}
            Err(SupplierConfigError::MissingTaxNumber) => {
                // Whitespace-after-trim case is already covered by the
                // `is_empty` branch above; this arm is theoretically
                // unreachable but the explicit match keeps the closed-vocab
                // exhaustive (no `_` wildcard).
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

    // Lines block.
    if request.lines.is_empty() {
        errors.push(InvoicePreflightError::InvoiceLinesEmpty);
    } else {
        for (line_index, line) in request.lines.iter().enumerate() {
            if line.description.trim().is_empty() {
                errors.push(InvoicePreflightError::LineItemDescriptionEmpty { line_index });
            }
            if line.quantity == 0 {
                errors.push(InvoicePreflightError::LineItemQuantityZero { line_index });
            }
            if line.unit_price <= 0 {
                errors.push(InvoicePreflightError::LineItemUnitPriceNonPositive {
                    line_index,
                    actual: line.unit_price,
                });
            }
            if !ALLOWED_VAT_RATES_PERCENT.contains(&line.vat_rate_percent) {
                errors.push(InvoicePreflightError::LineItemVatRateUnknown {
                    line_index,
                    actual: line.vat_rate_percent,
                    allowed: ALLOWED_VAT_RATES_PERCENT,
                });
            }
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue_invoice::{CustomerJson, LineJson};
    use aberp_billing::Currency;

    fn good_line() -> LineJson {
        LineJson {
            description: "CNC machining service".to_string(),
            quantity: 1,
            unit_price: 10_000,
            vat_rate_percent: 27,
        }
    }

    fn good_request() -> IssueInvoiceRequest {
        IssueInvoiceRequest {
            customer: CustomerJson {
                tax_number: "87654321-2-13".to_string(),
                name: "Áben Consulting KFT.".to_string(),
            },
            lines: vec![good_line()],
            currency: Currency::Huf,
            series: None,
            bank_account_id: None,
        }
    }

    // ── per-variant arms ───────────────────────────────────────────────

    #[test]
    fn golden_valid_request_returns_empty_vec() {
        assert!(validate_invoice_preflight(&good_request()).is_empty());
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
        r.lines[0].quantity = 0;
        let errs = validate_invoice_preflight(&r);
        assert!(
            errs.contains(&InvoicePreflightError::LineItemQuantityZero { line_index: 0 }),
            "expected LineItemQuantityZero, got {errs:?}"
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
                tax_number: "12345".to_string(), // malformed
                name: "  ".to_string(),          // blank
            },
            lines: vec![
                LineJson {
                    description: "".to_string(), // empty
                    quantity: 0,                 // zero
                    unit_price: -1,              // negative
                    vat_rate_percent: 12,        // off-vocab
                },
                good_line(), // line 1 is fine
                LineJson {
                    description: "second bad line".to_string(),
                    quantity: 1,
                    unit_price: 10,
                    vat_rate_percent: 99, // off-vocab
                },
            ],
            currency: Currency::Huf,
            series: None,
            bank_account_id: None,
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
}
