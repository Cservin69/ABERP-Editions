//! NAV v3.0 `InvoiceData` XML serialization.
//!
//! Produces the XML body that NAV expects inside a `manageInvoice`
//! `invoiceData` element. The SOAP envelope wrapping (request signature,
//! token exchange, transactionId tracking) belongs to the NAV adapter PR;
//! this file is the structural payload only.
//!
//! # Scope (PR-5 minimum subset)
//!
//! Required elements only:
//!
//! - `invoiceNumber`, `invoiceIssueDate`
//! - `invoiceMain/invoice/invoiceHead/supplierInfo` (taxNumber, name, address)
//! - `invoiceHead/customerInfo` (customerVatStatus, customerTaxNumber, name)
//! - `invoiceLines/line[*]` with lineNumber, lineDescription,
//!   quantity, unitOfMeasure, unitPrice, lineNetAmount, lineVatRate,
//!   lineVatData
//! - `invoiceSummary/summaryNormal` (sum totals)
//!
//! # NOT in scope (deferred to follow-up PRs)
//!
//! - XSD validation at runtime (ADR-0021 deferred item, named trigger
//!   "first PR implementing schema-drift detection per ADR-0009 §1").
//! - Optional NAV fields (deliveryDate, paymentDate, currency, exchange
//!   rate, foreign-currency lines, vatRateExemption, vatRateOutOfScope,
//!   product-codes, line-modification-reference, invoiceReference for
//!   storno/modify chains).
//! - SOAP envelope with `xmlSignKey` / `xmlChangeKey` signing.
//!
//! # NAV-compatibility claim
//!
//! "NAV-compatible" here means structurally matches NAV v3.0 InvoiceData
//! by inspection. PR-5 does NOT run an XSD validator; that lands when the
//! XSD-validator crate is picked per ADR-0021 §Items deferred.

use aberp_billing::{
    huf_equivalent_round_half_even, Currency, Huf, LineItem, NavUnitOfMeasure, PaymentMethod,
    ProductUnit, RateMetadata, ReadyInvoice, SeriesCode, VatRateKind,
};
use anyhow::{anyhow, Context, Result};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// PR-97 / ADR-0048 — closed-vocab discriminant for the buyer's NAV
/// `customerVatStatus` value. Three variants mirror NAV v3.0's
/// `customerVatStatusType` (DOMESTIC / PRIVATE_PERSON / OTHER).
///
/// **v1 scope:** Domestic + PrivatePerson are fully wired. Other is
/// named in the enum so a wire body carrying `"Other"` still
/// deserialises, but every materialising surface (preflight, emitter)
/// loud-fails at the v1 boundary per ADR-0048 §7. v2 wires Other end-
/// to-end with EU community-VAT vs non-EU third-state-tax-id sub-
/// shapes.
///
/// Serde uses the Rust PascalCase variant names (`"Domestic"`,
/// `"PrivatePerson"`, `"Other"`) so the SPA's string-union mirror
/// reads literally — same shape as [`PartnerKind`]
/// (`apps/aberp/src/partners.rs`). The NAV wire emits the
/// SCREAMING_SNAKE token via [`Self::as_nav_token`] — `"DOMESTIC"`,
/// `"PRIVATE_PERSON"`, `"OTHER"` — pinned by an emit test so a Rust
/// variant rename cannot silently drift the wire byte.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum CustomerVatStatus {
    /// Hungarian taxable entity. Today's universal default — the only
    /// branch the ABERP issuance path supported pre-PR-97. NAV wire
    /// REQUIRES `<customerVatData>` (structured `<customerTaxNumber>`)
    /// + `<customerAddress>` for this status.
    Domestic,
    /// Hungarian or foreign natural-person buyer (magánszemély). NAV
    /// wire FORBIDS `<customerVatData>` for this status; `<customerName>`
    /// is required; `<customerAddress>` is optional at the wire layer
    /// (Hungarian invoice law still requires it on the printed PDF —
    /// ADR-0048 §3 open-question #5 lands on "name always, address
    /// optional" for v1).
    PrivatePerson,
    /// Non-Hungarian buyer (EU community VAT or non-EU third-state
    /// tax-id). v1 named-defers this branch per ADR-0048 §7; the
    /// preflight emits [`CustomerVatStatusOtherNotSupportedV1`] BEFORE
    /// the emitter can be reached, and the NAV emitter itself
    /// loud-fails if it materialises this variant.
    Other,
}

impl CustomerVatStatus {
    /// Render the SCREAMING_SNAKE NAV wire token for this status —
    /// `"DOMESTIC"` / `"PRIVATE_PERSON"` / `"OTHER"`. Pinned by the
    /// `customer_vat_status_serde_round_trip` + the byte-verbatim
    /// emit pins so a Rust variant rename cannot silently drift the
    /// wire byte.
    pub fn as_nav_token(&self) -> &'static str {
        match self {
            CustomerVatStatus::Domestic => "DOMESTIC",
            CustomerVatStatus::PrivatePerson => "PRIVATE_PERSON",
            CustomerVatStatus::Other => "OTHER",
        }
    }

    /// Storage round-trip — DuckDB stores the PascalCase variant name
    /// (`"Domestic"` / `"PrivatePerson"` / `"Other"`) so the column
    /// reads as the same string the SPA wire body carries. Mirrors
    /// [`PartnerKind::as_db_str`] for symmetry.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            CustomerVatStatus::Domestic => "Domestic",
            CustomerVatStatus::PrivatePerson => "PrivatePerson",
            CustomerVatStatus::Other => "Other",
        }
    }

    /// Storage round-trip — inverse of [`Self::as_db_str`]. Returns
    /// `None` on an unrecognised input (the row read path loud-fails
    /// via a typed error rather than silently defaulting).
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "Domestic" => Some(CustomerVatStatus::Domestic),
            "PrivatePerson" => Some(CustomerVatStatus::PrivatePerson),
            "Other" => Some(CustomerVatStatus::Other),
            _ => None,
        }
    }
}

impl Default for CustomerVatStatus {
    /// Pre-PR-97 wire bodies (and pre-PR-97 partner rows) implicitly
    /// behaved as Domestic. Preserve that posture as the serde default
    /// so back-compat reads do NOT drift on a missing field.
    fn default() -> Self {
        CustomerVatStatus::Domestic
    }
}

/// Supplier + customer party data that the billing module does not own.
/// Supplied by the input JSON in PR-5; will move into its own module
/// when the customer module ships.
#[derive(Debug, Clone)]
pub struct NavParties {
    pub supplier: SupplierInfo,
    pub customer: CustomerInfo,
}

#[derive(Debug, Clone)]
pub struct SupplierInfo {
    pub tax_number: String,
    pub name: String,
    pub address_country_code: String,
    pub address_postal_code: String,
    pub address_city: String,
    pub address_street: String,
}

/// PR-50 / session-70 — decomposed Hungarian tax number per NAV
/// `Online Számla` v3.0 schema. The canonical wire form is
/// `xxxxxxxx-y-zz`: 8-digit base taxpayer id, 1-digit VAT code,
/// 2-digit county code. NAV's `<supplierTaxNumber>` is NOT a flat
/// string — it carries three required sub-elements (`<taxpayerId>`
/// + `<vatCode>` + `<countyCode>`), and the submit endpoint loud-
/// fails any body that emits the flat shape.
///
/// Held as raw strings (not `u32` + `u8` + `u8`) so the renderer
/// preserves byte-verbatim what the operator typed — leading zeros
/// in `taxpayerId` (none currently allocated by NAV, but the field is
/// 8 digits and a future allocation could carry a leading zero) and
/// `countyCode` survive the round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HungarianTaxNumber {
    /// 8 ASCII digits — the base taxpayer id surfaced inside
    /// `<taxpayerId>`. Validated for shape only; NAV owns the
    /// allocation registry.
    pub taxpayer_id: String,
    /// 1 ASCII digit — the VAT code surfaced inside `<vatCode>`.
    /// `1` = non-VAT-group taxpayer; `2` = VAT-group member;
    /// `3` = group representative; `4` = group internal. The
    /// renderer does not interpret the value, only its shape.
    pub vat_code: String,
    /// 2 ASCII digits — the county code surfaced inside
    /// `<countyCode>`. NAV publishes the registry separately;
    /// shape-validated here, semantically validated server-side at
    /// submit time.
    pub county_code: String,
}

/// PR-50 / session-70 — typed loud-fail error for supplier-config
/// validation. Surfaces at TWO points:
///
/// 1. `issue_from_parsed`'s pre-render guard — issuance refuses to
///    burn a sequence number when supplier data is malformed, so
///    the audit ledger never carries a half-issued invoice that
///    couldn't be submitted (CLAUDE.md rule 12, fail loud).
/// 2. `serve::handle_issue_invoice`'s route-layer validation — the
///    SPA receives a typed 400 body the operator can act on instead
///    of a 500 `internal error` that names nothing.
///
/// The variants pin distinct failure modes so the route handler can
/// craft a per-variant operator-actionable message; the SPA renders
/// the discriminant verbatim (the `Display` impl IS the
/// operator-facing message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupplierConfigError {
    /// `tax_number` field is empty after trim. The form field /
    /// `supplier.taxNumber` JSON property is required.
    MissingTaxNumber,
    /// `tax_number` failed `parse_hungarian_tax_number` shape
    /// validation. Carries the raw input for the error message so
    /// the operator sees exactly what was rejected.
    MalformedTaxNumber {
        /// The raw value the operator supplied — surfaced verbatim
        /// in the loud-fail message so the operator can spot the
        /// typo (missing dash, extra digit, etc.).
        input: String,
        /// One-line "what's wrong" diagnostic — appended to the
        /// `MalformedTaxNumber` Display surface so the message
        /// names the specific shape miss (length / non-digit
        /// character / dash placement).
        reason: &'static str,
    },
}

impl std::fmt::Display for SupplierConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupplierConfigError::MissingTaxNumber => {
                write!(
                    f,
                    "supplier tax number (ADÓSZÁM) is required \
                     (expected Hungarian shape `xxxxxxxx-y-zz`, e.g. `24904362-2-41`)"
                )
            }
            SupplierConfigError::MalformedTaxNumber { input, reason } => {
                write!(
                    f,
                    "supplier tax number `{input}` is not a valid Hungarian \
                     ADÓSZÁM ({reason}; expected `xxxxxxxx-y-zz`, \
                     e.g. `24904362-2-41`)"
                )
            }
        }
    }
}

impl std::error::Error for SupplierConfigError {}

/// PR-50 / session-70 — decompose a Hungarian ADÓSZÁM string in the
/// canonical `xxxxxxxx-y-zz` form into its three NAV-required sub-
/// elements. Validates:
///
///   - Three dash-separated segments (8 + 1 + 2 chars).
///   - Each segment is ASCII-digits-only.
///
/// Does NOT validate the semantic registry (taxpayer-id allocation,
/// county-code registry, vat-code value-range) — those live with
/// NAV's submit endpoint, which loud-fails server-side on a value
/// that's well-shaped but unallocated. Shape-validation here catches
/// the common operator-form-typo failure mode (missing dash, extra
/// digit) BEFORE the audit ledger burns a sequence number.
pub fn parse_hungarian_tax_number(input: &str) -> Result<HungarianTaxNumber, SupplierConfigError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(SupplierConfigError::MissingTaxNumber);
    }
    let segments: Vec<&str> = trimmed.split('-').collect();
    if segments.len() != 3 {
        return Err(SupplierConfigError::MalformedTaxNumber {
            input: trimmed.to_string(),
            reason: "expected three dash-separated segments",
        });
    }
    let (tp, vc, cc) = (segments[0], segments[1], segments[2]);
    if tp.len() != 8 {
        return Err(SupplierConfigError::MalformedTaxNumber {
            input: trimmed.to_string(),
            reason: "taxpayerId segment must be exactly 8 digits",
        });
    }
    if vc.len() != 1 {
        return Err(SupplierConfigError::MalformedTaxNumber {
            input: trimmed.to_string(),
            reason: "vatCode segment must be exactly 1 digit",
        });
    }
    if cc.len() != 2 {
        return Err(SupplierConfigError::MalformedTaxNumber {
            input: trimmed.to_string(),
            reason: "countyCode segment must be exactly 2 digits",
        });
    }
    for (seg, name) in [(tp, "taxpayerId"), (vc, "vatCode"), (cc, "countyCode")] {
        if !seg.chars().all(|c| c.is_ascii_digit()) {
            // Static reason picks the segment name without per-input alloc.
            let reason: &'static str = match name {
                "taxpayerId" => "taxpayerId segment must be ASCII digits only",
                "vatCode" => "vatCode segment must be an ASCII digit",
                _ => "countyCode segment must be ASCII digits only",
            };
            return Err(SupplierConfigError::MalformedTaxNumber {
                input: trimmed.to_string(),
                reason,
            });
        }
    }
    Ok(HungarianTaxNumber {
        taxpayer_id: tp.to_string(),
        vat_code: vc.to_string(),
        county_code: cc.to_string(),
    })
}

/// PR-50 / session-70 — supplier-info shape guard, called from
/// `issue_from_parsed` BEFORE any DB write and from
/// `serve::handle_issue_invoice` BEFORE dispatching to the issuance
/// pipeline. Inverts the prior "issuance succeeds, submit hours
/// later discovers garbage XML" failure mode (the bug Ervin hit on
/// 2026-05-25, INV-default/00001).
///
/// Today the surface is the tax-number shape — the supplier name +
/// address fields already get empty-string checks at the route
/// layer (`serve::validate_issue_request`). When PR-51 (the
/// SetupWizard's seller-config persistence) lands, this guard
/// extends to "seller.toml exists at `~/.aberp/<tenant>/seller.toml`"
/// without callers changing.
pub fn validate_supplier_info(supplier: &SupplierInfo) -> Result<(), SupplierConfigError> {
    let _ = parse_hungarian_tax_number(&supplier.tax_number)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct CustomerInfo {
    /// PR-97 / ADR-0048 — closed-vocab buyer-kind discriminant. Drives
    /// whether [`Self::tax_number`] is required (Domestic) or forbidden
    /// (PrivatePerson) and whether [`Self::address`] is required at the
    /// NAV wire layer. Backward-compat default (`Domestic`) keeps every
    /// pre-PR-97 fixture's behaviour unchanged.
    pub customer_vat_status: CustomerVatStatus,
    /// PR-97 / ADR-0048 — nullable for PrivatePerson buyers (NAV
    /// forbids `<customerVatData>` under PRIVATE_PERSON). For Domestic
    /// the upstream preflight + partner-form validation guarantee
    /// `Some(_)`; a Domestic + `None` reaching the emitter is a
    /// programmer-error loud-fail.
    pub tax_number: Option<String>,
    pub name: String,
    /// PR-77 / session-101 — NAV v3.0 business-rule
    /// `CUSTOMER_DATA_EXPECTED` requires `<customerAddress>` whenever
    /// `<customerVatStatus>` is non-PRIVATE_PERSON. PR-97 / ADR-0048
    /// — `Option<_>` because PRIVATE_PERSON tolerates absence at the
    /// wire layer (the print-PDF rule is enforced separately at the
    /// PDF render boundary). DOMESTIC + `None` is caught at preflight
    /// (`issue_preflight::CustomerAddressMissing`) BEFORE the sequence
    /// is burned, and at validator time as a defence-in-depth pin.
    pub address: Option<CustomerAddress>,
}

/// PR-77 / session-101 — structured customer address mirroring
/// `<customerAddress><common:simpleAddress>` per NAV v3.0 schema.
/// Mirrors [`SupplierInfo`]'s address fields shape. `country_code` is
/// ISO 3166-1 alpha-2 (`HU` for Hungarian DOMESTIC buyers — every
/// path that wires this struct today). Closed-vocab country + the
/// `Magyarország`-alias normalisation are named-deferred per the
/// PR-77 handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerAddress {
    pub country_code: String,
    pub postal_code: String,
    pub city: String,
    pub street: String,
}

/// Chain-link reference data for a STORNO or MODIFY operation —
/// [`render_storno_data`] / [`render_modification_data`] (PR-10 / PR-11,
/// ADR-0023 / ADR-0024). Pinpoints the base invoice and the chain index
/// this operation asserts. The XML emitter renders these into an
/// `<invoiceReference>` block inside `<invoice>` (positioned BEFORE
/// `<invoiceHead>` per NAV v3.0 schema).
///
/// S381/F1 — in NAV v3.0 the `<invoiceReference>` element
/// (`InvoiceReferenceType`) has EXACTLY three children
/// (`originalInvoiceNumber`, `modifyWithoutMaster`, `modificationIndex`);
/// the `<modificationIssueDate>` the MODIFY struct used to carry existed
/// only in v2.0 and is schema-illegal in v3.0, so it was removed. The
/// MODIFY and STORNO bodies are therefore structurally identical at the
/// `<invoiceReference>` level — the wire operation (CREATE/STORNO/MODIFY)
/// is declared on the SOAP envelope, derived from the audit ledger by
/// `submission_queue::operation_for_invoice`, NOT sniffed from the body.
///
/// S391/B collapsed the two formerly field-identical structs
/// (`StornoReference` / `ModificationReference`) into this single type;
/// the two names survive as [`StornoReference`] / [`ModificationReference`]
/// aliases so each render function's signature still reads with intent.
#[derive(Debug, Clone)]
pub struct ChainOperationReference {
    /// Base invoice's NAV-facing number — formatted as `<series>/<5-digit-seq>`
    /// (e.g. `INV-default/00007`). The caller constructs this from the
    /// base invoice row's series + sequence_number; see `issue_storno::run`
    /// / `issue_modification::run`.
    pub base_invoice_number: String,
    /// `<modificationIndex>` allocated by the chain walker (ADR-0023 §4 /
    /// ADR-0024 §7) — starts at 1, increments per chain entry. The MODIFY
    /// walker walks both `InvoiceStornoIssued` AND `InvoiceModificationIssued`
    /// entries against the same base, so the index is globally unique
    /// across the chain regardless of per-kind order.
    pub modification_index: u32,
    /// S369/S384/S391 — the `<lineNumberReference>` OFFSET for this
    /// operation's CREATE-mode lines: the count of ALL lines NAV already
    /// holds on this chain (the base PLUS every SAVED prior modification).
    /// The new lines continue PAST this offset so they never reuse a line
    /// number NAV already recorded on the base OR a prior modification —
    /// NAV ABORTs with `INVOICE_LINE_ALREADY_EXISTS` otherwise (observed in
    /// prod, S370).
    ///
    /// Pre-S384 the storno used only the BASE's line count (a base-only
    /// storno reverses only the base). S384/F5 generalised the storno
    /// offset to base + every saved modification; S391/A did the same for
    /// the MODIFY path (a modify-after-modify chain must offset past every
    /// saved prior modification, not just the base). The caller folds this
    /// from each chain member's on-disk NAV XML via
    /// [`count_invoice_lines_from_xml`] / [`read_invoice_lines_from_xml`]
    /// (+ [`crate::issue_storno::total_prior_chain_line_count`]) — the
    /// canonical record of what NAV holds on file — the same on-disk-read
    /// discipline `base_invoice_number` uses (S184).
    pub base_line_count: usize,
}

/// STORNO-shape alias of [`ChainOperationReference`] (S391/B). Names the
/// input to [`render_storno_data`] / [`render_storno_data_with_number`].
pub type StornoReference = ChainOperationReference;

/// MODIFY-shape alias of [`ChainOperationReference`] (S391/B). Names the
/// input to [`render_modification_data`] /
/// [`render_modification_data_with_number`].
pub type ModificationReference = ChainOperationReference;

const NAV_NS_DATA: &str = "http://schemas.nav.gov.hu/OSA/3.0/data";
const NAV_NS_BASE: &str = "http://schemas.nav.gov.hu/OSA/3.0/base";
/// NAV v3.0 annul namespace per ADR-0025 §"Surfaced conflict 1"'s
/// chosen reading. The `manageInvoice` body uses `OSA/3.0/data`;
/// the `manageAnnulment` counterpart by NAV's namespace convention
/// uses `OSA/3.0/annul`. Verification deferred to first NAV-testbed
/// annulment POST (the future submit-annulment PR).
const NAV_NS_ANNUL: &str = "http://schemas.nav.gov.hu/OSA/3.0/annul";

/// Technical-annulment reference data for [`render_annulment_data`]
/// (PR-12, ADR-0025). The annulment is **not** a chain operation:
/// no `<invoiceReference>` block, no `modificationIndex`. The
/// payload-side analogue is
/// `audit_payloads::InvoiceTechnicalAnnulmentRequestedPayload`;
/// this struct carries only the fields that surface on the
/// `<InvoiceAnnulment>` XML body (the audit payload additionally
/// carries the operator-decision idempotency + prior transaction id
/// for the audit-evidence bundle, which do not appear on the wire).
#[derive(Debug, Clone)]
pub struct AnnulmentReference {
    /// Base invoice's NAV-facing number — same shape + caller
    /// discipline as [`StornoReference::base_invoice_number`] /
    /// [`ModificationReference::base_invoice_number`]. Becomes the
    /// `<annulmentReference>` text content.
    pub base_invoice_number: String,
    /// NAV annulment code in canonical wire form — one of
    /// `ERRATIC_DATA` / `ERRATIC_INVOICE_NUMBER` /
    /// `ERRATIC_INVOICE_ISSUE_DATE` /
    /// `ERRATIC_ELECTRONIC_HASH_VALUE`. The caller converts the
    /// clap-ValueEnum form to the wire form via
    /// `cli::AnnulmentCode::to_wire` before constructing this
    /// struct.
    pub annulment_code: &'static str,
    /// Operator-supplied reason text — escaped by `quick_xml`'s
    /// text writer the same way every other text-element write
    /// goes through.
    pub reason: String,
}

/// Render `<InvoiceData>` to bytes. The invoice number is built from the
/// series code and the allocator-burned sequence number: `INV-default/00042`.
///
/// # Currency + rate metadata (PR-44δ / ADR-0037 §1.b)
///
/// `currency` carries the typed `Currency` (HUF or EUR per ADR-0037 §3's
/// closed vocab). `rate_metadata` MUST be `Some(_)` when `currency` is a
/// non-HUF variant (ADR-0037 §4 invariant C1's wire-side counterpart) and
/// SHOULD be `None` for HUF. The function loud-fails on
/// `Currency::Eur` + `None` rather than silently emitting a HUF-shaped
/// body for an EUR invoice — CLAUDE.md rule 12.
///
/// For HUF the wire body is byte-near-identical to the pre-PR-44δ shape
/// (`<currencyCode>HUF</currencyCode>`, all per-VAT-rate + invoice-level
/// `*HUF` amounts equal to their non-HUF siblings) with one deliberate
/// change: `<exchangeRate>` now serializes as `1.000000` (6 decimals per
/// ADR-0037 §1.c + C11) rather than the prior `1`. The NAV XSD accepts
/// both; the 6-decimal form pins the C11 precision invariant uniformly
/// across HUF and EUR.
///
/// For EUR the rate stamped at PR-44γ is read from `rate_metadata.rate`
/// (a `rust_decimal::Decimal` — full precision MNB returned). Per-VAT-rate
/// and invoice-level HUF amounts are computed via the same
/// `huf_equivalent_round_half_even` helper PR-44γ uses for the per-invoice
/// gross-total stamp; the rate is NOT re-fetched here (per the
/// session-52 brief — drift from the audit ledger's stamped rate is the
/// failure mode this design rules out).
pub fn render_invoice_data(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<Vec<u8>> {
    // S160 — the thin wrapper keeps its pre-S160 signature (≈40 test call
    // sites depend on it); it passes the `Transfer` default, byte-identical
    // to the pre-S160 hardcoded `<paymentMethod>TRANSFER</...>`. Production
    // callers use the `_with_number` variant to pass the operator's choice.
    render_invoice_data_with_number(
        invoice,
        series_code,
        parties,
        currency,
        rate_metadata,
        PaymentMethod::Transfer,
        None,
    )
}

/// PR-89 — variant of [`render_invoice_data`] that accepts a
/// pre-rendered `invoice_number` override. When `Some(s)`, `s` is
/// emitted as the `<invoiceNumber>` element verbatim — this is the
/// path the PR-89 operator-configurable [`crate::numbering`] template
/// flows through. When `None`, the renderer falls back to the
/// pre-PR-89 `format!("{}/{:05}", series_code, seq)` shape for
/// backwards-compat with the existing test corpus + any caller that
/// has not yet adopted the template path.
///
/// The renderer does NOT validate the override string against the NAV
/// `invoiceNumber` XSD charset — that gate lives at config time in
/// [`crate::numbering::validate_template`]. By the time a string
/// reaches this function it is guaranteed to be NAV-legal (loud-fail
/// in the SPA save endpoint refuses an illegal template before any
/// invoice is issued under it).
pub fn render_invoice_data_with_number(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
    payment_method: PaymentMethod,
    invoice_number_override: Option<&str>,
) -> Result<Vec<u8>> {
    ensure_rate_metadata_invariant(currency, rate_metadata)?;

    let mut buf: Vec<u8> = Vec::new();
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    // <?xml version="1.0" encoding="UTF-8"?>
    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("XML declaration")?;

    // Root element with namespaces.
    let mut root = BytesStart::new("InvoiceData");
    root.push_attribute(("xmlns", NAV_NS_DATA));
    root.push_attribute(("xmlns:common", NAV_NS_BASE));
    w.write_event(Event::Start(root))
        .context("write <InvoiceData>")?;

    let legacy = format!("{}/{:05}", series_code.as_str(), invoice.sequence_number,);
    let invoice_number = invoice_number_override.unwrap_or(&legacy);
    text_element(&mut w, "invoiceNumber", invoice_number)?;
    // PR-84 — three NAV date fields share one formatter
    // (`nav_date_string`). `invoiceIssueDate` is the server-stamped
    // immutable date; `invoiceDeliveryDate` is the operator-chosen
    // REGULATORY date (drives VAT-period assignment); `paymentDate` is
    // the operator-chosen payment deadline.
    let issue_date = nav_date_string(invoice.issue_date.date());
    let delivery_date = nav_date_string(invoice.delivery_date);
    let payment_date = nav_date_string(invoice.payment_deadline);
    text_element(&mut w, "invoiceIssueDate", &issue_date)?;
    // <completenessIndicator> — PR-76. NAV v3.0 InvoiceData XSD names this
    // as a REQUIRED element positioned between `<invoiceIssueDate>` and
    // `<invoiceMain>`. Always `false` for ABERP: the dual-purpose flag
    // distinguishes "submitting data only — printed invoice is the
    // primary record" (`true`) from "submitting both the data AND the
    // invoice itself" (`false`); ABERP issues electronic invoices through
    // NAV's data-submission API, so `false` is correct. Missing this
    // element was the SCHEMA_VIOLATION NAV-test rejected invoice 17 with.
    text_element(&mut w, "completenessIndicator", "false")?;

    // <invoiceMain>
    w.write_event(Event::Start(BytesStart::new("invoiceMain")))?;
    w.write_event(Event::Start(BytesStart::new("invoice")))?;

    // <invoiceHead>
    w.write_event(Event::Start(BytesStart::new("invoiceHead")))?;
    write_supplier(&mut w, &parties.supplier)?;
    write_customer(&mut w, &parties.customer)?;
    write_invoice_detail(
        &mut w,
        &delivery_date,
        &payment_date,
        currency,
        rate_metadata,
        payment_method,
    )?;
    w.write_event(Event::End(BytesEnd::new("invoiceHead")))?;

    // <invoiceLines> — plain new invoice: NORMAL lines, no
    // <lineModificationReference> (no <invoiceReference> at the head).
    // S369 — initial issuance numbers lines from 1 (offset 0).
    write_lines(&mut w, &invoice.lines, currency, rate_metadata, None, 0)?;

    // <invoiceSummary>
    write_summary(&mut w, &invoice.lines, currency, rate_metadata)?;

    w.write_event(Event::End(BytesEnd::new("invoice")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceMain")))?;
    w.write_event(Event::End(BytesEnd::new("InvoiceData")))?;

    Ok(buf)
}

/// PR-84 — uniform calendar-date formatter for the three NAV date
/// fields (`<invoiceIssueDate>`, `<invoiceDeliveryDate>`, `<paymentDate>`).
/// NAV's XSD pins `xs:date` (YYYY-MM-DD); format manually rather than
/// depending on `time::Iso8601`'s const-generic configuration so the
/// formatting code matches the existing `OffsetDateTime`-based emit shape
/// byte-for-byte. The three callers (`render_invoice_data`,
/// `render_storno_data`, `render_modification_data`) share one helper so
/// a future format drift surfaces in one place.
fn nav_date_string(date: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day(),
    )
}

/// Refuse a non-HUF currency without rate metadata; pinned by the
/// `eur_render_without_rate_metadata_loud_fails` test. The mirror of
/// the issuance-side ADR-0037 §4 invariant C1 check that PR-44γ
/// enforces in `aberp_billing::allocate_in_tx`'s pre-flight.
fn ensure_rate_metadata_invariant(
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    match (currency, rate_metadata) {
        (Currency::Huf, _) => Ok(()),
        (_, Some(_)) => Ok(()),
        (other, None) => Err(anyhow!(
            "non-HUF invoice currency {} requires rate_metadata at NAV body render time \
             (ADR-0037 §4 invariant C1, NAV-body side)",
            other.iso_code()
        )),
    }
}

/// Render the storno's `<InvoiceData>` to bytes (PR-10, ADR-0023).
///
/// Two differences from [`render_invoice_data`]:
///
/// 1. An `<invoiceReference>` block appears inside `<invoice>`,
///    positioned BEFORE `<invoiceHead>` per NAV v3.0 schema. Carries
///    `originalInvoiceNumber` + `modifyWithoutMaster` (always `false`
///    for PR-10 — ADR-0023 §4 names the migrated-base path that
///    would set this `true` and explicitly defers it) +
///    `modificationIndex`.
///
/// 2. Line and summary amounts are **negated** per NAV's storno
///    convention. Negation is done by constructing a parallel
///    `Vec<LineItem>` with negated `quantity` (S381/F3 — NAV spec
///    §2.5.1 negates the line quantities, not the unit price);
///    `net_total` / `vat_amount` / `gross_total` cascade to negative
///    naturally because the same multiplications now run against a
///    negative `quantity`. This keeps the line-writer logic shared with
///    [`render_invoice_data`] instead of forking a parallel
///    `write_storno_lines` — CLAUDE.md rule 2 (no speculative
///    abstractions).
///
/// The `invoice` argument carries the STORNO's own sequence number
/// (the storno is itself an invoice with its own allocator slot per
/// ADR-0009 §6 / ADR-0023 §3); `storno_reference.base_invoice_number`
/// names what is being cancelled.
pub fn render_storno_data(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    storno_reference: &StornoReference,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<Vec<u8>> {
    // S160 — thin wrapper passes the `Transfer` default (see
    // [`render_invoice_data`]). Production storno issuance uses the
    // `_with_number` variant to inherit the base invoice's payment method.
    //
    // S384/F5 — base-only reversal: the reversal source IS the storno's
    // own (base-derived) lines. The chain-aware fold (base + saved
    // modifications) lives in `issue_storno`, which calls the
    // `_with_number` variant directly.
    render_storno_data_with_number(
        invoice,
        series_code,
        parties,
        storno_reference,
        currency,
        rate_metadata,
        PaymentMethod::Transfer,
        None,
        &invoice.lines,
    )
}

/// PR-89 — variant of [`render_storno_data`] with a pre-rendered
/// `invoice_number` override. See [`render_invoice_data_with_number`]'s
/// doc-comment for the override semantics; this is the same path for
/// storno chains. The `storno_reference.base_invoice_number` is NOT
/// re-rendered here — the caller composes it (the storno-issue route
/// renders the BASE's number from the template at storno-issue time).
pub fn render_storno_data_with_number(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    storno_reference: &StornoReference,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
    payment_method: PaymentMethod,
    invoice_number_override: Option<&str>,
    // S384/F5 — the FULL set of prior chain lines this storno must
    // reverse: the base's lines PLUS every SAVED prior modification's
    // lines (un-negated; this renderer negates them). For a base-only
    // storno (no saved modifications) the caller passes `&invoice.lines`,
    // which is byte-identical to the pre-S384 behaviour. The caller
    // (`issue_storno`) folds these from each chain member's on-disk NAV
    // XML via `read_invoice_lines_from_xml`. `storno_reference.
    // base_line_count` MUST equal this slice's length (it is the
    // `<lineNumberReference>` offset that points the reversal lines past
    // ALL prior chain lines).
    reversal_source_lines: &[LineItem],
) -> Result<Vec<u8>> {
    // PR-44γ.1 — same C1-wire-side invariant the fresh-issuance renderer
    // enforces: a non-HUF storno without inherited rate metadata is a
    // loud-fail (the chain-currency inheritance path supplies the
    // metadata; missing metadata means the caller bypassed
    // `inherit_rate_metadata_for_chain`).
    ensure_rate_metadata_invariant(currency, rate_metadata)?;

    let mut buf: Vec<u8> = Vec::new();
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("XML declaration")?;

    let mut root = BytesStart::new("InvoiceData");
    root.push_attribute(("xmlns", NAV_NS_DATA));
    root.push_attribute(("xmlns:common", NAV_NS_BASE));
    w.write_event(Event::Start(root))
        .context("write <InvoiceData> (storno)")?;

    // Storno's OWN invoice number — the cancellation is itself an invoice.
    let legacy = format!("{}/{:05}", series_code.as_str(), invoice.sequence_number);
    let invoice_number = invoice_number_override.unwrap_or(&legacy);
    text_element(&mut w, "invoiceNumber", invoice_number)?;
    // PR-84 — STORNO chains inherit pre-PR-84 behaviour (delivery +
    // payment mirror the chain-storno's issue date). The storno UX
    // does not surface operator-supplied date pickers yet; `ReadyInvoice`
    // carries `delivery_date == payment_deadline == issue_date.date()`
    // from `issue_storno.rs`. Same `nav_date_string` formatter as the
    // fresh-issuance renderer so a format drift surfaces in one place.
    let issue_date = nav_date_string(invoice.issue_date.date());
    let delivery_date = nav_date_string(invoice.delivery_date);
    let payment_date = nav_date_string(invoice.payment_deadline);
    text_element(&mut w, "invoiceIssueDate", &issue_date)?;
    // <completenessIndicator> — PR-76. NAV v3.0 schema-required element
    // between `<invoiceIssueDate>` and `<invoiceMain>`; same posture as
    // [`render_invoice_data`] (always `false` — ABERP data-submits, it
    // does not assert the printed invoice replaces the data record).
    text_element(&mut w, "completenessIndicator", "false")?;

    w.write_event(Event::Start(BytesStart::new("invoiceMain")))?;
    w.write_event(Event::Start(BytesStart::new("invoice")))?;

    // <invoiceReference> — STORNO-only. Position: direct child of
    // <invoice>, BEFORE <invoiceHead>, per NAV v3.0 schema.
    write_invoice_reference(&mut w, storno_reference)?;

    // <invoiceHead> reuses the standard supplier/customer/detail
    // section writers — the storno's parties and detail block are
    // identical in shape to a fresh invoice's. The NAV-side operation
    // (CREATE vs STORNO vs MODIFY) is set on the SOAP envelope at
    // submit time, not inside <InvoiceData>; submit_invoice.rs
    // detects the storno shape by the presence of <invoiceReference>
    // (PR-10 F20).
    w.write_event(Event::Start(BytesStart::new("invoiceHead")))?;
    write_supplier(&mut w, &parties.supplier)?;
    write_customer(&mut w, &parties.customer)?;
    // PR-44γ.1 — currency + rate metadata inherited from base per
    // ADR-0037 §4 invariant C6 (built by the chain caller via
    // `invoice_currency_metadata::inherit_rate_metadata_for_chain`).
    write_invoice_detail(
        &mut w,
        &delivery_date,
        &payment_date,
        currency,
        rate_metadata,
        payment_method,
    )?;
    w.write_event(Event::End(BytesEnd::new("invoiceHead")))?;

    // <invoiceLines> with negated amounts. Negate by constructing a
    // parallel Vec with negated quantity (S381/F3 — NAV spec §2.5.1
    // negates quantities, not unit price); net/vat/gross cascade through
    // `LineItem::net_total` etc. unchanged.
    //
    // S384/F5 — the reversal set is `reversal_source_lines` (base lines
    // PLUS every SAVED prior modification's lines), NOT just
    // `invoice.lines`. A MODIFY-then-STORNO chain otherwise leaves the
    // modification's net un-reversed → NAV's
    // `INCONSISTENT_MODIFICATION_DATA_*_NOT_ZERO*` WARN class.
    let negated_lines: Vec<LineItem> = reversal_source_lines.iter().map(negate_line).collect();
    // Storno carries <invoiceReference>, so every line MUST carry a
    // <lineModificationReference> (ADR-0049 §NAV emit / NAV
    // LINE_MODIFICATION_EXPECTED). lineOperation is CREATE per NAV's
    // INVALID_LINE_OPERATION business rule (S184) — see
    // `CHAIN_LINE_OPERATION`. S369/S384 — lineNumberReference CONTINUES
    // PAST every prior chain line (`base_line_count` == total prior
    // line count == `negated_lines.len()`) so the CREATE lines do not
    // collide with any line number NAV already recorded on the base OR a
    // prior modification (NAV INVOICE_LINE_ALREADY_EXISTS, S370 prod
    // incident).
    write_lines(
        &mut w,
        &negated_lines,
        currency,
        rate_metadata,
        Some(CHAIN_LINE_OPERATION),
        storno_reference.base_line_count,
    )?;
    write_summary(&mut w, &negated_lines, currency, rate_metadata)?;

    w.write_event(Event::End(BytesEnd::new("invoice")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceMain")))?;
    w.write_event(Event::End(BytesEnd::new("InvoiceData")))?;

    Ok(buf)
}

/// Render the modification's `<InvoiceData>` to bytes (PR-11,
/// ADR-0024).
///
/// Structurally parallel to [`render_storno_data`] with two
/// differences that follow from ADR-0024 §3 + §4:
///
/// 1. The `<invoiceReference>` block is byte-identical to the STORNO
///    one (S381/F1 — NAV v3.0 `InvoiceReferenceType` has exactly three
///    children; the v2.0-only `<modificationIssueDate>` was removed).
///    The wire operation (CREATE vs STORNO vs MODIFY) is declared on
///    the SOAP envelope, derived from the audit ledger by
///    `submission_queue::operation_for_invoice` (NOT sniffed from the
///    body, which can no longer distinguish STORNO from MODIFY).
///
/// 2. Line and summary amounts are **NOT negated.** The modification
///    is a **full-replace** body per ADR-0024 §4 — it carries the
///    new effective invoice values, not a delta. The line writers are
///    reused against the input invoice's lines directly, so this
///    function shares `write_lines` / `write_summary` with
///    [`render_invoice_data`] (and, by happenstance, with
///    [`render_storno_data`] via that storno function's negated
///    parallel `Vec`).
///
/// The `invoice` argument carries the MODIFICATION's own sequence
/// number (the modification is itself an invoice with its own
/// allocator slot per ADR-0009 §6 + ADR-0024 §5);
/// `modification_reference.base_invoice_number` names what is being
/// corrected.
pub fn render_modification_data(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    modification_reference: &ModificationReference,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<Vec<u8>> {
    // S160 — thin wrapper passes the `Transfer` default (see
    // [`render_invoice_data`]). Production modification issuance uses the
    // `_with_number` variant to inherit the base invoice's payment method.
    render_modification_data_with_number(
        invoice,
        series_code,
        parties,
        modification_reference,
        currency,
        rate_metadata,
        PaymentMethod::Transfer,
        None,
    )
}

/// PR-89 — variant of [`render_modification_data`] with a pre-rendered
/// `invoice_number` override. Same override semantics as
/// [`render_invoice_data_with_number`].
pub fn render_modification_data_with_number(
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    modification_reference: &ModificationReference,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
    payment_method: PaymentMethod,
    invoice_number_override: Option<&str>,
) -> Result<Vec<u8>> {
    // PR-44γ.1 — same C1-wire-side invariant the fresh-issuance renderer
    // enforces: a non-HUF modification without inherited rate metadata
    // loud-fails.
    ensure_rate_metadata_invariant(currency, rate_metadata)?;

    let mut buf: Vec<u8> = Vec::new();
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("XML declaration")?;

    let mut root = BytesStart::new("InvoiceData");
    root.push_attribute(("xmlns", NAV_NS_DATA));
    root.push_attribute(("xmlns:common", NAV_NS_BASE));
    w.write_event(Event::Start(root))
        .context("write <InvoiceData> (modification)")?;

    // Modification's OWN invoice number — the correction is itself an invoice.
    let legacy = format!("{}/{:05}", series_code.as_str(), invoice.sequence_number);
    let invoice_number = invoice_number_override.unwrap_or(&legacy);
    text_element(&mut w, "invoiceNumber", invoice_number)?;
    // PR-84 — MODIFICATION chains inherit pre-PR-84 behaviour. The
    // modification's `ReadyInvoice` carries
    // `delivery_date == payment_deadline == issue_date.date()` from
    // `issue_modification.rs` because the modification UX does not
    // surface date pickers yet.
    let issue_date = nav_date_string(invoice.issue_date.date());
    let delivery_date = nav_date_string(invoice.delivery_date);
    let payment_date = nav_date_string(invoice.payment_deadline);
    text_element(&mut w, "invoiceIssueDate", &issue_date)?;
    // <completenessIndicator> — PR-76. NAV v3.0 schema-required element
    // between `<invoiceIssueDate>` and `<invoiceMain>`; same posture as
    // [`render_invoice_data`] / [`render_storno_data`].
    text_element(&mut w, "completenessIndicator", "false")?;

    w.write_event(Event::Start(BytesStart::new("invoiceMain")))?;
    w.write_event(Event::Start(BytesStart::new("invoice")))?;

    // <invoiceReference> — Position: direct child of <invoice>, BEFORE
    // <invoiceHead>, per NAV v3.0 schema (same position as the STORNO
    // block). S381/F1 — the MODIFY `<invoiceReference>` is now
    // byte-identical to the STORNO one (the v2.0-only
    // `<modificationIssueDate>` was removed), so it reuses
    // `write_invoice_reference` directly rather than a duplicate writer.
    write_invoice_reference(
        &mut w,
        &StornoReference {
            base_invoice_number: modification_reference.base_invoice_number.clone(),
            modification_index: modification_reference.modification_index,
            base_line_count: modification_reference.base_line_count,
        },
    )?;

    // <invoiceHead> reuses the standard supplier/customer/detail
    // section writers — same posture as the STORNO emitter; party +
    // detail data is the modification's own (corrected) values.
    w.write_event(Event::Start(BytesStart::new("invoiceHead")))?;
    write_supplier(&mut w, &parties.supplier)?;
    write_customer(&mut w, &parties.customer)?;
    // PR-44γ.1 — currency + rate metadata inherited from base per
    // ADR-0037 §4 invariant C6.
    write_invoice_detail(
        &mut w,
        &delivery_date,
        &payment_date,
        currency,
        rate_metadata,
        payment_method,
    )?;
    w.write_event(Event::End(BytesEnd::new("invoiceHead")))?;

    // <invoiceLines> + <invoiceSummary> — NOT negated. Full-replace
    // per ADR-0024 §4; the modification's `invoice.lines` already
    // carry the new effective values. The modification carries
    // <invoiceReference>, so every line MUST carry a
    // <lineModificationReference> (ADR-0049 §NAV emit) — same
    // LINE_MODIFICATION_EXPECTED gap the storno emitter had. S369 —
    // the full-replace CREATE lines continue PAST the base's line
    // count (`base_line_count` offset) so they do not collide with
    // the base's recorded line numbers (NAV INVOICE_LINE_ALREADY_EXISTS).
    write_lines(
        &mut w,
        &invoice.lines,
        currency,
        rate_metadata,
        Some(CHAIN_LINE_OPERATION),
        modification_reference.base_line_count,
    )?;
    write_summary(&mut w, &invoice.lines, currency, rate_metadata)?;

    w.write_event(Event::End(BytesEnd::new("invoice")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceMain")))?;
    w.write_event(Event::End(BytesEnd::new("InvoiceData")))?;

    Ok(buf)
}

/// Render `<InvoiceAnnulment>` to bytes (PR-12, ADR-0025).
///
/// **Structurally distinct** from [`render_invoice_data`] /
/// [`render_storno_data`] / [`render_modification_data`]:
///
/// - **Different root element + namespace.** Root is
///   `<InvoiceAnnulment>` in the `OSA/3.0/annul` namespace (the
///   `manageAnnulment` endpoint's body shape) per ADR-0025 §
///   "Surfaced conflict 1". Verification deferred to first NAV-
///   testbed annulment POST.
/// - **No `<invoiceMain>` / `<invoiceHead>` / lines / summary.** A
///   technical annulment is NOT itself an invoice; it carries only
///   the four metadata fields (reference + timestamp + code +
///   reason).
/// - **`<annulmentTimestamp>` is server-clock-only.** Per ADR-0025
///   §4 — annulment timestamp is a technical not legal field; no
///   operator-supplied date arg. Captured at render time as ISO 8601
///   UTC (`YYYY-MM-DDTHH:MM:SSZ`). If NAV's testbed requires the
///   compressed `YYYYMMDDhhmmss` form (which is what
///   `requestTimestamp` uses in the SOAP header), the change is a
///   one-line formatter swap and the wire shape per ADR-0025 §
///   "Open questions" is the named trigger.
///
/// The `annulment_reference` argument carries the BASE invoice's
/// NAV-facing number (the thing being annulled), the wire-form
/// annulment code, and the operator's reason text. The caller is
/// responsible for building `base_invoice_number` the same way
/// `issue_storno.rs` / `issue_modification.rs` does.
pub fn render_annulment_data(annulment_reference: &AnnulmentReference) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("XML declaration")?;

    let mut root = BytesStart::new("InvoiceAnnulment");
    root.push_attribute(("xmlns", NAV_NS_ANNUL));
    root.push_attribute(("xmlns:common", NAV_NS_BASE));
    w.write_event(Event::Start(root))
        .context("write <InvoiceAnnulment>")?;

    text_element(
        &mut w,
        "annulmentReference",
        &annulment_reference.base_invoice_number,
    )?;
    // Server-clock-only timestamp per ADR-0025 §4. ISO 8601 UTC
    // (`YYYY-MM-DDTHH:MM:SSZ`). Formatted manually rather than
    // depending on `time::Iso8601`'s const-generic configuration —
    // same posture as `render_invoice_data`'s manual date format.
    let now = time::OffsetDateTime::now_utc();
    let timestamp = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    );
    text_element(&mut w, "annulmentTimestamp", &timestamp)?;
    text_element(&mut w, "annulmentCode", annulment_reference.annulment_code)?;
    text_element(&mut w, "annulmentReason", &annulment_reference.reason)?;

    w.write_event(Event::End(BytesEnd::new("InvoiceAnnulment")))?;

    Ok(buf)
}

/// Negate a `LineItem` for storno emission. S381/F3 — NAV spec §2.5.1
/// is explicit that a storno carries the original line items "with
/// opposite signs for all **quantities**" (so the total quantities tend
/// to be negative); the `<unitPrice>` stays positive. Pre-S381 the
/// negation lived in `unit_price` instead — a letter-of-spec divergence
/// that triggered no coded NAV WARN (arithmetic stays consistent within
/// the 1%/1-unit tolerance either way) but diverged from what NAV's
/// analysts/auditors expect to see in the warehouse. The cascading
/// `net_total` / `vat_amount` / `gross_total` are unchanged in
/// magnitude and still negative (`(-quantity) × unit_price` equals the
/// old `quantity × (-unit_price)`), so line/summary totals are
/// byte-identical; only the `<quantity>`/`<unitPrice>` sign placement
/// moves.
fn negate_line(line: &LineItem) -> LineItem {
    LineItem {
        description: line.description.clone(),
        quantity: -line.quantity,
        unit_price: line.unit_price,
        vat_rate_basis_points: line.vat_rate_basis_points,
        // ADR-0101 — preserve the base line's VAT rate-kind verbatim
        // through negation. Like the description and VAT rate, the kind is
        // not part of the amount-sign reversal; carrying it forward keeps
        // the storno's `<lineVatRate>` choice element identical to the base.
        vat_rate_kind: line.vat_rate_kind,
        // PR-82 — preserve the base's per-line `note` verbatim through
        // negation. The note is recipient-facing metadata, NOT part of
        // the amount-sign reversal; carrying it forward keeps the
        // storno's stored line shape consistent with the printed PDF.
        // (NAV XML emission still does not consume the note — see the
        // never-leak invariant in `adr/0042-invoice-notes-never-in-nav-xml.md`.)
        note: line.note.clone(),
        // S159 — preserve the base line's unit verbatim through negation
        // so the storno's correction line emits the SAME `<unitOfMeasure>`
        // as the original. Unit, like the description and VAT rate, is not
        // part of the amount-sign reversal.
        unit: line.unit.clone(),
    }
}

/// Write the `<invoiceReference>` chain-link block for STORNO **and**
/// MODIFY bodies. S381/F1 — NAV v3.0's `InvoiceReferenceType` has
/// exactly three children (`originalInvoiceNumber`, `modifyWithoutMaster`,
/// `modificationIndex`); the v2.0-only `<modificationIssueDate>` was
/// removed, so the two chain shapes are now byte-identical here and
/// share this one writer (the separate `write_modification_reference`
/// was deleted).
///
/// Always emits `modifyWithoutMaster=false`: ADR-0023 §4 names the
/// `queryInvoiceChainDigest` path for migrated-from-Billingo bases
/// (the case where `modifyWithoutMaster=true` would be the right
/// value) and explicitly defers it. When the migrated-base path
/// lands, this function gains a `modify_without_master: bool` field
/// on `StornoReference`; for PR-10 the constant-false value is
/// loud-pinned here.
fn write_invoice_reference(
    w: &mut Writer<&mut Vec<u8>>,
    storno_reference: &StornoReference,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("invoiceReference")))?;
    text_element(
        w,
        "originalInvoiceNumber",
        &storno_reference.base_invoice_number,
    )?;
    text_element(w, "modifyWithoutMaster", "false")?;
    text_element(
        w,
        "modificationIndex",
        &storno_reference.modification_index.to_string(),
    )?;
    w.write_event(Event::End(BytesEnd::new("invoiceReference")))?;
    Ok(())
}

// ── Section writers ───────────────────────────────────────────────────

fn write_supplier(w: &mut Writer<&mut Vec<u8>>, s: &SupplierInfo) -> Result<()> {
    // PR-50 / session-70 — NAV `Online Számla` v3.0 schema requires
    // `<supplierTaxNumber>` to carry three structured sub-elements
    // (`<taxpayerId>` + `<vatCode>` + `<countyCode>`), NOT a flat
    // dashed string. The flat shape ships clean past `nav-xsd-
    // validator`'s pre-2026-05-25 reading of the schema but loud-
    // fails server-side when NAV's submit endpoint rejects the body
    // with `<supplierTaxNumber> missing <taxpayerId>`. Decompose at
    // render time so issuance + submit agree on the wire shape.
    let parsed = parse_hungarian_tax_number(&s.tax_number)
        .map_err(|e| anyhow!("supplier tax number invalid at NAV-XML render time: {e}"))?;
    w.write_event(Event::Start(BytesStart::new("supplierInfo")))?;
    w.write_event(Event::Start(BytesStart::new("supplierTaxNumber")))?;
    common_element(w, "taxpayerId", &parsed.taxpayer_id)?;
    common_element(w, "vatCode", &parsed.vat_code)?;
    common_element(w, "countyCode", &parsed.county_code)?;
    w.write_event(Event::End(BytesEnd::new("supplierTaxNumber")))?;
    text_element(w, "supplierName", &s.name)?;
    write_address(w, "supplierAddress", s)?;
    w.write_event(Event::End(BytesEnd::new("supplierInfo")))?;
    Ok(())
}

fn write_customer(w: &mut Writer<&mut Vec<u8>>, c: &CustomerInfo) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("customerInfo")))?;
    text_element(w, "customerVatStatus", c.customer_vat_status.as_nav_token())?;

    // PR-97 / ADR-0048 §4 — `<customerVatData>` emission is conditional
    // on the closed-vocab status. Domestic: REQUIRED structured tax
    // block (PR-50 / PR-66 hold). PrivatePerson: FORBIDDEN — NAV's
    // CUSTOMER_DATA_EXPECTED rule fires on its presence under
    // PRIVATE_PERSON. Other: v1 named-deferred per ADR-0048 §7 — the
    // emitter loud-fails here so a misrouted Other body cannot escape
    // ABERP onto the wire.
    match c.customer_vat_status {
        CustomerVatStatus::Domestic => {
            let tax_number = c.tax_number.as_deref().ok_or_else(|| {
                anyhow!(
                    "Domestic customer requires tax_number at NAV-XML render time \
                     — preflight + partner-form validation should have caught this upstream \
                     (ADR-0048 §4)"
                )
            })?;
            let parsed = parse_hungarian_tax_number(tax_number)
                .map_err(|e| anyhow!("customer tax number invalid at NAV-XML render time: {e}"))?;
            w.write_event(Event::Start(BytesStart::new("customerVatData")))?;
            w.write_event(Event::Start(BytesStart::new("customerTaxNumber")))?;
            common_element(w, "taxpayerId", &parsed.taxpayer_id)?;
            common_element(w, "vatCode", &parsed.vat_code)?;
            common_element(w, "countyCode", &parsed.county_code)?;
            w.write_event(Event::End(BytesEnd::new("customerTaxNumber")))?;
            w.write_event(Event::End(BytesEnd::new("customerVatData")))?;
        }
        CustomerVatStatus::PrivatePerson => {
            // Intentional no-emit. NAV business-rule forbids
            // `<customerVatData>` under PRIVATE_PERSON; pinned by
            // `emitter_writes_customer_info_under_private_person_omits_vat_data`
            // and the validator's symmetric ForbiddenChildUnderStatus rule.
        }
        CustomerVatStatus::Other => {
            return Err(anyhow!(
                "ADR-0048 §7: Other-status customer emit is v1 named-deferred \
                 (use Domestic or PrivatePerson; foreign-buyer support lands in v2)"
            ));
        }
    }

    // Session-154 (ADR-0048 amendment 2026-05-29) — `<customerName>` and
    // `<customerAddress>` are emitted on the NAV wire for every buyer kind
    // EXCEPT PRIVATE_PERSON. NAV's business-tier rule
    // `CUSTOMER_DATA_NOT_EXPECTED` ("Magánszemély vevő adatai nem adhatók
    // meg.") rejects a PrivatePerson body carrying either field, ABORTING
    // the submit. §169 of the Áfa tv. mandates buyer name + address on the
    // *printed* invoice — that governs the PDF, NOT the NAV wire — so the
    // PDF still renders both unconditionally (PR-148/150 preserved). The
    // unconditional emit added in sessions 148/150 leaked PDF logic into the
    // wire; this re-separates the two surfaces. Position is AFTER any
    // `<customerVatData>` per the v3.0 XSD CustomerInfoType ordering.
    if !matches!(c.customer_vat_status, CustomerVatStatus::PrivatePerson) {
        text_element(w, "customerName", &c.name)?;
        if let Some(address) = c.address.as_ref() {
            write_customer_address(w, address)?;
        }
    }
    w.write_event(Event::End(BytesEnd::new("customerInfo")))?;
    Ok(())
}

/// PR-77 / session-101 — emit `<customerAddress><common:simpleAddress>`.
/// Mirrors [`write_address`] but takes a typed [`CustomerAddress`]
/// rather than a [`SupplierInfo`] (the two structs do not share an
/// address shape — supplier's address fields live flat on the struct
/// per PR-50's pre-CustomerAddress posture; threading a shared trait
/// here would be a CLAUDE.md rule 2 abstraction over two call sites).
fn write_customer_address(w: &mut Writer<&mut Vec<u8>>, address: &CustomerAddress) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("customerAddress")))?;
    w.write_event(Event::Start(BytesStart::new("common:simpleAddress")))?;
    common_element(w, "countryCode", &address.country_code)?;
    common_element(w, "postalCode", &address.postal_code)?;
    common_element(w, "city", &address.city)?;
    common_element(w, "additionalAddressDetail", &address.street)?;
    w.write_event(Event::End(BytesEnd::new("common:simpleAddress")))?;
    w.write_event(Event::End(BytesEnd::new("customerAddress")))?;
    Ok(())
}

fn write_address(w: &mut Writer<&mut Vec<u8>>, tag: &str, s: &SupplierInfo) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag.to_string())))?;
    w.write_event(Event::Start(BytesStart::new("common:simpleAddress")))?;
    common_element(w, "countryCode", &s.address_country_code)?;
    common_element(w, "postalCode", &s.address_postal_code)?;
    common_element(w, "city", &s.address_city)?;
    common_element(w, "additionalAddressDetail", &s.address_street)?;
    w.write_event(Event::End(BytesEnd::new("common:simpleAddress")))?;
    w.write_event(Event::End(BytesEnd::new(tag.to_string())))?;
    Ok(())
}

fn write_invoice_detail(
    w: &mut Writer<&mut Vec<u8>>,
    delivery_date: &str,
    payment_deadline: &str,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
    payment_method: PaymentMethod,
) -> Result<()> {
    // ADR-0037 §1.b — `<currencyCode>` is the ISO 4217 code; `<exchangeRate>`
    // is the rate at exactly 6 decimal places per the NAV `Online Számla`
    // XSD (confirmed 2026-05-23 legal cleanup). For HUF the conceptual rate
    // is 1 (HUF-per-HUF); we serialize the same 6-decimal form
    // (`1.000000`) so the C11 precision invariant holds uniformly across
    // HUF and EUR. The validator accepts both forms (`ensure_numeric_amount`
    // is shape-agnostic on decimal-places); the uniform precision is the
    // load-bearing posture pin.
    //
    // PR-84 — `invoiceDeliveryDate` (Teljesítési dátum) and `paymentDate`
    // (Fizetési határidő) are now operator-supplied and may differ from
    // each other AND from `<invoiceIssueDate>`. The two YYYY-MM-DD
    // strings come from `ReadyInvoice.delivery_date` and
    // `ReadyInvoice.payment_deadline` at the caller — both fields are
    // server-validated calendar dates (the wire parse loud-fails on a
    // malformed date BEFORE we reach this writer). The pre-PR-84 path
    // silently mirrored `<invoiceIssueDate>` for both fields; that bug
    // (regulatory: would mis-file the VAT period for any back- or
    // forward-dated invoice) is closed by this signature.
    let exchange_rate = match (currency, rate_metadata) {
        (Currency::Huf, _) => "1.000000".to_string(),
        (_, Some(meta)) => format_rate_six_decimals(&meta.rate),
        (_, None) => unreachable!("ensure_rate_metadata_invariant ruled this out"),
    };
    w.write_event(Event::Start(BytesStart::new("invoiceDetail")))?;
    text_element(w, "invoiceCategory", "NORMAL")?;
    text_element(w, "invoiceDeliveryDate", delivery_date)?;
    text_element(w, "currencyCode", currency.iso_code())?;
    text_element(w, "exchangeRate", &exchange_rate)?;
    // S160 — operator-selected payment method (Fizetési mód), snapshotted
    // per invoice (ADR-0050). Pre-S160 the emit hardcoded `TRANSFER`; the
    // `PaymentMethod::default()` (== `Transfer`) carried by pre-S160
    // side-stored `input.json` bodies (via `#[serde(default)]`) keeps that
    // path byte-identical. NAV's `paymentMethodType` is a CLOSED enum with
    // no free-text companion — there is no `<paymentMethodOwn>` (unlike
    // `<unitOfMeasureOwn>`), so `Other` ("Egyéb") is the catch-all.
    text_element(w, "paymentMethod", payment_method.nav_token())?;
    text_element(w, "paymentDate", payment_deadline)?;
    text_element(w, "invoiceAppearance", "ELECTRONIC")?;
    w.write_event(Event::End(BytesEnd::new("invoiceDetail")))?;
    Ok(())
}

/// Serialize a `rust_decimal::Decimal` rate at exactly 6 decimal places
/// per ADR-0037 §1.c + §4 invariant C11. `Decimal`'s `Display` impl
/// honours the precision specifier (`{:.6}`) and pads with trailing
/// zeros — exactly what the NAV XSD `decimal(6)` shape requires. Pinned
/// by `rate_serializes_at_six_decimals` in the round-trip test file.
fn format_rate_six_decimals(rate: &Decimal) -> String {
    format!("{:.6}", rate)
}

/// Format an `i64` of minor units (EUR cents) as a two-decimal EUR
/// amount string. Used by [`write_lines`] / [`write_summary`] on the
/// EUR branch — the wire body's `lineNetAmount` / `vatRateNetAmount` /
/// `invoiceNetAmount` etc. carry the native-currency amount; for EUR
/// that is `cents / 100` with two decimal places. Negative amounts
/// (storno chain children, future) prepend a single `-`.
fn format_minor_units_two_decimals(minor_units: i64) -> String {
    let sign = if minor_units < 0 { "-" } else { "" };
    let abs = minor_units.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// HUF equivalent of an invoice-currency minor-unit amount under the
/// supplied rate-metadata. For `Currency::Huf` the amount is already in
/// whole forints — no conversion. For non-HUF currencies the amount is
/// in cents and we apply [`huf_equivalent_round_half_even`] using the
/// PR-44γ-stamped rate (read from the persisted `RateMetadata`, NOT
/// re-fetched) per ADR-0037 §1.c + §4 invariant C11.
fn huf_equivalent_for(
    minor_units: i64,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<i64> {
    match (currency, rate_metadata) {
        (Currency::Huf, _) => Ok(minor_units),
        (_, Some(meta)) => {
            huf_equivalent_round_half_even(minor_units, &meta.rate).ok_or_else(|| {
                anyhow!(
                    "HUF-equivalent conversion overflowed i64 for {} cents at rate {}",
                    minor_units,
                    meta.rate
                )
            })
        }
        (other, None) => Err(anyhow!(
            "non-HUF currency {} requires rate_metadata at HUF-equivalent computation",
            other.iso_code()
        )),
    }
}

/// Serialize a per-line / per-VAT-rate / invoice-level amount in its
/// native currency: integer forints for HUF; two-decimal EUR cents for
/// EUR. The two branches diverge at the wire boundary; the underlying
/// `i64` carries minor units in both cases (the EUR amount lives inside
/// a `Huf(cents)` wrapper today per PR-44γ's interim posture — see
/// `apps/aberp/src/issue_invoice.rs::finalize_rate`).
fn format_native_amount(minor_units: i64, currency: Currency) -> String {
    match currency {
        Currency::Huf => minor_units.to_string(),
        _ => format_minor_units_two_decimals(minor_units),
    }
}

/// `<lineOperation>` value for STORNO and MODIFY chain bodies (ADR-0049
/// §NAV emit). NAV's `LINE_OPERATION` enum is `{CREATE, MODIFY}` per the
/// v3.0 XSD, but the runtime business rule is stricter: every line of a
/// chain body (storno OR modification) MUST be `CREATE`. NAV rejects
/// `MODIFY` with business-rule `INVALID_LINE_OPERATION` and the
/// operator-visible message "Módosító vagy érvénytelenítő számláról
/// beküldött adatszolgáltatásban a lineOperation elem értéknek minden
/// esetben „CREATE"-nek kell lennie" — "On a data report submitted from
/// a modifying or invalidating invoice, the lineOperation element's
/// value must always be CREATE."
///
/// S184 — was `"MODIFY"` (S156 / ADR-0049's initial guess; that session
/// 156 doc explicitly flagged the value for NAV-XSD confirmation).
/// Confirmed `"CREATE"` from a live NAV ABORTED ack (transaction
/// `5EF1QF3Y1W9HIFNW`, base `TEST-TEST-ABERP/2026/0042`) whose
/// `businessValidationMessages` named the rule above verbatim. NAV's
/// data warehouse stores each invoice line as a fact row: a chain body
/// is a NEW submission that introduces fresh fact rows (the negation in
/// storno's case; the full-replace values in modification's case) so
/// those fresh rows are `CREATE`-d, not `MODIFY`-d. `MODIFY` would only
/// apply to the (extremely rare) meta-correction of a previously-
/// submitted chain line itself.
const CHAIN_LINE_OPERATION: &str = "CREATE";

/// Emit the per-line `<lineModificationReference>` block (ADR-0049
/// §NAV emit). Present only on chain bodies (storno / modification) —
/// any invoice carrying `<invoiceReference>` at the head must carry this
/// on EVERY `<line>`, or NAV rejects with `LINE_MODIFICATION_EXPECTED`.
///
/// Position: NAV's `LineType` sequence places `lineModificationReference`
/// as the SECOND element — directly AFTER `<lineNumber>` and BEFORE
/// `<lineExpressionIndicator>`. (ADR-0049 / the session-155 memo phrased
/// it "first child"; the NAV XSD requires `<lineNumber>` to be the
/// literal first child, so the reference is emitted immediately after it.
/// Session 156 surfaced this conflict and followed the XSD ordering.)
///
/// `line_number_reference` is the line's position on the ORIGINAL invoice.
/// For a storno that is 1:1 with the base, and for the full-replace
/// modification, this equals the line's own 1-based ordinal.
fn write_line_modification_reference(
    w: &mut Writer<&mut Vec<u8>>,
    line_number_reference: u32,
    line_operation: &str,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("lineModificationReference")))?;
    text_element(w, "lineNumberReference", &line_number_reference.to_string())?;
    text_element(w, "lineOperation", line_operation)?;
    w.write_event(Event::End(BytesEnd::new("lineModificationReference")))?;
    Ok(())
}

/// Write `<invoiceLines>`. `line_operation` is `Some(op)` only for chain
/// bodies (storno / modification — those carrying `<invoiceReference>`);
/// when set, every `<line>` carries a `<lineModificationReference>` after
/// its `<lineNumber>`. `None` (plain new invoice) emits a NORMAL line with
/// no reference (ADR-0049 §NAV emit).
///
/// `line_number_offset` shifts ONLY the chain-body `<lineNumberReference>`
/// so it points PAST the base invoice's line numbers — it does NOT touch
/// `<lineNumber>`. `<lineNumber>` is always document-local: 1-based and
/// monotonic within EACH invoice (`1..=n`), per NAV's
/// `LINE_NUMBER_NOT_SEQUENTIAL` business rule (S372 prod incident — S369
/// over-shifted `<lineNumber>` too, ABORTing the submit). For initial
/// issuance the offset is `0` (reference equals line number). For storno /
/// modification chain children it is the base invoice's line count, so a
/// CREATE operation's `<lineNumberReference>` never reuses a line number
/// NAV already recorded on the base — NAV's `INVOICE_LINE_ALREADY_EXISTS`
/// business rule ABORTs the submit otherwise (prod incident, S370 root
/// cause). The safety property lives here, NOT in caller discipline: every
/// chain body routes through this one writer, so a future chain emitter
/// cannot silently emit a colliding `<lineNumberReference>`.
fn write_lines(
    w: &mut Writer<&mut Vec<u8>>,
    lines: &[LineItem],
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
    line_operation: Option<&str>,
    line_number_offset: usize,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("invoiceLines")))?;
    text_element(w, "mergedItemIndicator", "false")?;
    for (ordinal, line) in lines.iter().enumerate() {
        // `<lineNumber>` is the DOCUMENT-LOCAL position — always 1-based and
        // monotonic WITHIN this invoice (`1..=n`), NEVER shifted; NAV's
        // LINE_NUMBER_NOT_SEQUENTIAL business rule ABORTs the submit otherwise
        // (S372 prod incident). Only the chain-body `<lineNumberReference>`
        // carries `line_number_offset` so it points PAST the base invoice's
        // line numbers.
        let line_number = (ordinal + 1) as u32;
        let line_number_reference = (line_number_offset + ordinal + 1) as u32;
        w.write_event(Event::Start(BytesStart::new("line")))?;
        text_element(w, "lineNumber", &line_number.to_string())?;
        // <lineModificationReference> — chain-body-only (storno / modify).
        // Positioned after <lineNumber> per NAV LineType ordering.
        if let Some(op) = line_operation {
            write_line_modification_reference(w, line_number_reference, op)?;
        }
        text_element(w, "lineExpressionIndicator", "false")?;
        text_element(w, "lineDescription", &line.description)?;
        // S157 — decimal quantity. NAV's `<quantity>` is a dot-separated
        // decimal (the XSD validator's `ensure_numeric_amount` accepts
        // `1.5` and `1`). `.normalize()` strips the trailing zeros a
        // DECIMAL(18,6) read-back carries (`1.500000` → `1.5`, `3.000000`
        // → `3`) so the wire stays minimal; `Decimal::to_string` always
        // emits `.` regardless of locale.
        text_element(w, "quantity", &line.quantity.normalize().to_string())?;
        // S159 — the line's unit of measure. NAV's `LineType` places
        // `<unitOfMeasure>` here (after `<quantity>`, before `<unitPrice>`),
        // and `<unitOfMeasureOwn>` is valid ONLY when `<unitOfMeasure>` is
        // the literal `OWN`. The closed-vocab `Nav` variants emit their
        // token directly; `Own(text)` emits `OWN` + the free-text element
        // (text XML-escaped by `text_element`); `None` (freetext line or a
        // pre-S159 / DB-reconstructed line) falls back to PIECE. The XSD
        // validator (`nav-xsd-validator::walk_line`) enforces the
        // OWN ↔ unitOfMeasureOwn pairing.
        match line.unit.as_ref() {
            Some(ProductUnit::Nav(unit)) => {
                text_element(w, "unitOfMeasure", unit.nav_token())?;
            }
            Some(ProductUnit::Own(text)) => {
                text_element(w, "unitOfMeasure", "OWN")?;
                text_element(w, "unitOfMeasureOwn", text)?;
            }
            None => {
                text_element(w, "unitOfMeasure", "PIECE")?;
            }
        }
        text_element(
            w,
            "unitPrice",
            &format_native_amount(line.unit_price.as_i64(), currency),
        )?;

        let net = line
            .net_total()
            .context("line net_total overflow during XML render")?;
        let vat = line
            .vat_amount()
            .context("line vat_amount overflow during XML render")?;
        let gross = line
            .gross_total()
            .context("line gross_total overflow during XML render")?;

        w.write_event(Event::Start(BytesStart::new("lineAmountsNormal")))?;
        write_line_net(w, net, currency, rate_metadata)?;
        write_line_vat_rate(w, line.vat_rate_kind, line.vat_rate_basis_points)?;
        write_line_vat_amount(w, vat, currency, rate_metadata)?;
        write_line_gross(w, gross, currency, rate_metadata)?;
        w.write_event(Event::End(BytesEnd::new("lineAmountsNormal")))?;

        w.write_event(Event::End(BytesEnd::new("line")))?;
    }
    w.write_event(Event::End(BytesEnd::new("invoiceLines")))?;
    Ok(())
}

fn write_line_net(
    w: &mut Writer<&mut Vec<u8>>,
    net: Huf,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    let huf = huf_equivalent_for(net.as_i64(), currency, rate_metadata)?;
    w.write_event(Event::Start(BytesStart::new("lineNetAmountData")))?;
    text_element(
        w,
        "lineNetAmount",
        &format_native_amount(net.as_i64(), currency),
    )?;
    text_element(w, "lineNetAmountHUF", &huf.to_string())?;
    w.write_event(Event::End(BytesEnd::new("lineNetAmountData")))?;
    Ok(())
}
/// ADR-0101 — the NAV `<lineVatRate>` / `<vatRate>` choice a
/// [`VatRateKind`] resolves to. This is the SINGLE source of truth for the
/// `(element, case, reason)` triple (ADR-0101 §3.2 / CLAUDE.md rule 5 —
/// code answers the deterministic transform; the model stores only the
/// kind). Consumed by BOTH the per-line emit (`write_line_vat_rate`) and
/// the invoice-level summary mirror (`write_vat_rate`) so a line and its
/// `summaryByVatRate` bucket can never disagree (§3.4 — a line/summary
/// category mismatch is itself a NAV rejection).
enum VatRateChoice {
    /// `<vatPercentage>{rate}</vatPercentage>` — the numeric path,
    /// byte-identical to the pre-0101 emit.
    Percentage,
    /// `<vatExemption><case>{case}</case><reason>{reason}</reason></…>`.
    Exemption {
        case: &'static str,
        reason: &'static str,
    },
    /// `<vatOutOfScope><case>{case}</case><reason>{reason}</reason></…>`.
    OutOfScope {
        case: &'static str,
        reason: &'static str,
    },
    /// `<vatDomesticReverseCharge>true</vatDomesticReverseCharge>` — a
    /// boolean element with no case code.
    DomesticReverseCharge,
}

/// Map a [`VatRateKind`] to its NAV choice. Case codes CONFIRMED by Ervin
/// on the record (2026-07-15) — AAM / KBAET / EUFAD37 / the
/// `vatDomesticReverseCharge` boolean. `reason` is a statutory Hungarian
/// free-text string (NAV requires non-blank; it is NOT buyer PII — the
/// ADR-0042 notes firewall is unaffected). The named-deferred kinds
/// (ADR-0101 §3.1) `anyhow!` here as explicit not-yet markers (CLAUDE.md
/// rule 12) — they are unreachable in Session 1 because preflight rejects
/// every non-`Percent` kind before emit.
fn vat_rate_choice(kind: VatRateKind) -> Result<VatRateChoice> {
    Ok(match kind {
        VatRateKind::Percent => VatRateChoice::Percentage,
        VatRateKind::AamExempt => VatRateChoice::Exemption {
            case: "AAM",
            reason: "Alanyi adómentesség [Áfa tv. 187–196. §]",
        },
        VatRateKind::IntraCommunityGoods => VatRateChoice::Exemption {
            case: "KBAET",
            reason: "Közösségen belüli adómentes termékértékesítés [Áfa tv. 89. §]",
        },
        VatRateKind::IntraCommunityServiceReverse => VatRateChoice::OutOfScope {
            case: "EUFAD37",
            reason: "Áfa területi hatályán kívüli, fordított adózású ügylet [Áfa tv. 37. §]",
        },
        VatRateKind::DomesticReverseCharge => VatRateChoice::DomesticReverseCharge,
        // ── named-deferred (ADR-0101 §3.1 / §10.4) — unreachable in
        //    Session 1 (preflight shut door). Wiring one is a later PR that
        //    must confirm its own NAV case code first. ──
        other => {
            return Err(anyhow!(
                "VatRateKind::{other:?} is ADR-0101 named-deferred; NAV emit is not yet wired \
                 for it (confirm its NAV case code first)"
            ))
        }
    })
}

/// Write the inner choice element of a `<lineVatRate>` / `<vatRate>`.
/// Shared so the per-line emit and the summary mirror stay in lock-step.
fn write_vat_rate_choice(
    w: &mut Writer<&mut Vec<u8>>,
    kind: VatRateKind,
    vat_rate_basis_points: u16,
) -> Result<()> {
    match vat_rate_choice(kind)? {
        VatRateChoice::Percentage => {
            let rate_decimal = format!("{:.2}", vat_rate_basis_points as f64 / 10000.0);
            text_element(w, "vatPercentage", &rate_decimal)?;
        }
        VatRateChoice::Exemption { case, reason } => {
            w.write_event(Event::Start(BytesStart::new("vatExemption")))?;
            text_element(w, "case", case)?;
            text_element(w, "reason", reason)?;
            w.write_event(Event::End(BytesEnd::new("vatExemption")))?;
        }
        VatRateChoice::OutOfScope { case, reason } => {
            w.write_event(Event::Start(BytesStart::new("vatOutOfScope")))?;
            text_element(w, "case", case)?;
            text_element(w, "reason", reason)?;
            w.write_event(Event::End(BytesEnd::new("vatOutOfScope")))?;
        }
        VatRateChoice::DomesticReverseCharge => {
            text_element(w, "vatDomesticReverseCharge", "true")?;
        }
    }
    Ok(())
}

/// Write <vatRate> for summary (not <lineVatRate>). ADR-0101 — the summary
/// mirror: same choice shape as the line, keyed on the (single) invoice
/// kind so the `summaryByVatRate` bucket matches the lines (§3.4).
fn write_vat_rate(
    w: &mut Writer<&mut Vec<u8>>,
    kind: VatRateKind,
    vat_rate_basis_points: u16,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("vatRate")))?;
    write_vat_rate_choice(w, kind, vat_rate_basis_points)?;
    w.write_event(Event::End(BytesEnd::new("vatRate")))?;
    Ok(())
}
fn write_line_vat_rate(
    w: &mut Writer<&mut Vec<u8>>,
    kind: VatRateKind,
    vat_rate_basis_points: u16,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("lineVatRate")))?;
    write_vat_rate_choice(w, kind, vat_rate_basis_points)?;
    w.write_event(Event::End(BytesEnd::new("lineVatRate")))?;
    Ok(())
}

fn write_line_vat_amount(
    w: &mut Writer<&mut Vec<u8>>,
    vat: Huf,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    let huf = huf_equivalent_for(vat.as_i64(), currency, rate_metadata)?;
    w.write_event(Event::Start(BytesStart::new("lineVatData")))?;
    text_element(
        w,
        "lineVatAmount",
        &format_native_amount(vat.as_i64(), currency),
    )?;
    text_element(w, "lineVatAmountHUF", &huf.to_string())?;
    w.write_event(Event::End(BytesEnd::new("lineVatData")))?;
    Ok(())
}

fn write_line_gross(
    w: &mut Writer<&mut Vec<u8>>,
    gross: Huf,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    let huf = huf_equivalent_for(gross.as_i64(), currency, rate_metadata)?;
    w.write_event(Event::Start(BytesStart::new("lineGrossAmountData")))?;
    text_element(
        w,
        "lineGrossAmountNormal",
        &format_native_amount(gross.as_i64(), currency),
    )?;
    text_element(w, "lineGrossAmountNormalHUF", &huf.to_string())?;
    w.write_event(Event::End(BytesEnd::new("lineGrossAmountData")))?;
    Ok(())
}

fn write_summary(
    w: &mut Writer<&mut Vec<u8>>,
    lines: &[LineItem],
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> Result<()> {
    let mut net_total = Huf::ZERO;
    let mut vat_total = Huf::ZERO;
    let mut gross_total = Huf::ZERO;
    for (i, line) in lines.iter().enumerate() {
        net_total = net_total
            .checked_add(line.net_total().unwrap_or(Huf::ZERO))
            .with_context(|| format!("net overflow at line {i}"))?;
        vat_total = vat_total
            .checked_add(line.vat_amount().unwrap_or(Huf::ZERO))
            .with_context(|| format!("vat overflow at line {i}"))?;
        gross_total = gross_total
            .checked_add(line.gross_total().unwrap_or(Huf::ZERO))
            .with_context(|| format!("gross overflow at line {i}"))?;
    }

    // PR-44δ — ADR-0037 §1.c invoice-level HUF totals are the sum of
    // per-VAT-rate HUF amounts (NOT a fresh round_half_even on the
    // EUR invoice total). PR-5's single-`summaryByVatRate` posture
    // collapses everything into one VAT rate today; when a future PR
    // extends to multi-rate invoices the per-rate HUF amounts here
    // need to be summed before serializing the invoice-level *HUF.
    let net_total_huf = huf_equivalent_for(net_total.as_i64(), currency, rate_metadata)?;
    let vat_total_huf = huf_equivalent_for(vat_total.as_i64(), currency, rate_metadata)?;
    let gross_total_huf = huf_equivalent_for(gross_total.as_i64(), currency, rate_metadata)?;

    w.write_event(Event::Start(BytesStart::new("invoiceSummary")))?;
    w.write_event(Event::Start(BytesStart::new("summaryNormal")))?;
    // summaryByVatRate (one entry, assuming all lines share a rate; PR-5
    // does not yet group by rate — a follow-up extends this for
    // multi-rate invoices).
    if let Some(first) = lines.first() {
        w.write_event(Event::Start(BytesStart::new("summaryByVatRate")))?;
        // ADR-0101 §3.4 — the summary mirror keys on the first line's KIND
        // as well as its rate, so the `summaryByVatRate` bucket's choice
        // element matches the lines' (today's single-bucket posture assumes
        // all lines share a rate/kind). A line/summary category mismatch is
        // a NAV cross-field rejection.
        write_vat_rate(w, first.vat_rate_kind, first.vat_rate_basis_points)?;
        w.write_event(Event::Start(BytesStart::new("vatRateNetData")))?;
        text_element(
            w,
            "vatRateNetAmount",
            &format_native_amount(net_total.as_i64(), currency),
        )?;
        text_element(w, "vatRateNetAmountHUF", &net_total_huf.to_string())?;
        w.write_event(Event::End(BytesEnd::new("vatRateNetData")))?;
        w.write_event(Event::Start(BytesStart::new("vatRateVatData")))?;
        text_element(
            w,
            "vatRateVatAmount",
            &format_native_amount(vat_total.as_i64(), currency),
        )?;
        text_element(w, "vatRateVatAmountHUF", &vat_total_huf.to_string())?;
        w.write_event(Event::End(BytesEnd::new("vatRateVatData")))?;
        w.write_event(Event::Start(BytesStart::new("vatRateGrossData")))?;
        text_element(
            w,
            "vatRateGrossAmount",
            &format_native_amount(gross_total.as_i64(), currency),
        )?;
        text_element(w, "vatRateGrossAmountHUF", &gross_total_huf.to_string())?;
        w.write_event(Event::End(BytesEnd::new("vatRateGrossData")))?;
        w.write_event(Event::End(BytesEnd::new("summaryByVatRate")))?;
    }
    text_element(
        w,
        "invoiceNetAmount",
        &format_native_amount(net_total.as_i64(), currency),
    )?;
    text_element(w, "invoiceNetAmountHUF", &net_total_huf.to_string())?;
    text_element(
        w,
        "invoiceVatAmount",
        &format_native_amount(vat_total.as_i64(), currency),
    )?;
    text_element(w, "invoiceVatAmountHUF", &vat_total_huf.to_string())?;
    w.write_event(Event::End(BytesEnd::new("summaryNormal")))?;
    w.write_event(Event::Start(BytesStart::new("summaryGrossData")))?;
    text_element(
        w,
        "invoiceGrossAmount",
        &format_native_amount(gross_total.as_i64(), currency),
    )?;
    text_element(w, "invoiceGrossAmountHUF", &gross_total_huf.to_string())?;
    w.write_event(Event::End(BytesEnd::new("summaryGrossData")))?;
    w.write_event(Event::End(BytesEnd::new("invoiceSummary")))?;
    Ok(())
}
/// Write an element with the `common:` prefix (NAV v3.0 base namespace).
/// Used for elements defined in the common/base namespace that appear
/// inside data-namespace parents (e.g., <common:taxpayerId> inside
/// <supplierTaxNumber>).
fn common_element(w: &mut Writer<&mut Vec<u8>>, tag: &str, value: &str) -> Result<()> {
    let full_tag = format!("common:{}", tag);
    w.write_event(Event::Start(BytesStart::new(full_tag.clone())))?;
    w.write_event(Event::Text(BytesText::new(value)))?;
    w.write_event(Event::End(BytesEnd::new(full_tag)))?;
    Ok(())
}
fn text_element(w: &mut Writer<&mut Vec<u8>>, tag: &str, value: &str) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag.to_string())))?;
    w.write_event(Event::Text(BytesText::new(value)))?;
    w.write_event(Event::End(BytesEnd::new(tag.to_string())))?;
    Ok(())
}

/// S184 — read the BASE invoice's `<invoiceNumber>` element text from an
/// on-disk NAV InvoiceData XML file. This is the canonical record of
/// what NAV has on file for that invoice: the chain emitter (storno /
/// modification) MUST reference the BASE by the exact string NAV saw on
/// the original `manageInvoice` POST, or NAV rejects with the business-
/// rule `INVALID_INVOICE_REFERENCE` ("A módosítás vagy érvénytelenítés
/// olyan okiratra hivatkozik, amire vonatkozóan nem történt
/// adatszolgáltatás" — "the modification / invalidation references a
/// document for which no data has been reported"). Pre-S184 the chain
/// emitters re-derived the base's number via
/// `NumberingTemplate::render_for_build(base_year, base_seq)` — which
/// works IFF the seller.toml literal + `INVOICE_NUMBER_TEST_PREFIX` were
/// identical at base-issuance time AND chain-emit time. Any operator
/// edit to the seller.toml literal (or a build-prefix flip mid-stream)
/// silently drifts the reference. The on-disk XML is immune: it was
/// written at base-issuance and never re-rewritten, so its
/// `<invoiceNumber>` is exactly what NAV received.
///
/// Tolerates the `xmlns="…/OSA/3.0/data"` default-namespace declaration
/// on the root by matching on the local element name only (NAV's emit
/// is namespaced; our renderer also is). Loud-fails when:
///   - the file cannot be opened or read (operator deleted it — chain
///     issuance MUST fail loud rather than burn a sequence number
///     against an unrecoverable reference);
///   - the XML is malformed past quick_xml's lenient threshold;
///   - no `<invoiceNumber>` element appears before `</invoice>` of the
///     first invoice block (the base XML is tampered).
pub fn read_invoice_number_from_xml(path: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "read base NAV XML at {} to extract <invoiceNumber> for chain reference (S184)",
            path.display()
        )
    })?;
    let xml = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "base NAV XML at {} is not valid UTF-8 (S184)",
            path.display()
        )
    })?;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut depth: u32 = 0;
    loop {
        match reader.read_event().with_context(|| {
            format!(
                "parse base NAV XML at {} while seeking <invoiceNumber> (S184)",
                path.display()
            )
        })? {
            quick_xml::events::Event::Start(e) => {
                depth += 1;
                // Match by local name (strip any `prefix:` if NAV emit uses one).
                let name = e.name();
                let local = name.local_name();
                if local.as_ref() == b"invoiceNumber" {
                    let text = reader
                        .read_text(e.to_end().name())
                        .with_context(|| {
                            format!(
                                "read text of <invoiceNumber> in base NAV XML at {} (S184)",
                                path.display()
                            )
                        })?
                        .into_owned();
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        return Err(anyhow!(
                            "base NAV XML at {} has an empty <invoiceNumber> element (S184)",
                            path.display()
                        ));
                    }
                    return Ok(trimmed.to_string());
                }
            }
            quick_xml::events::Event::End(_) => {
                depth = depth.saturating_sub(1);
            }
            quick_xml::events::Event::Eof => {
                return Err(anyhow!(
                    "base NAV XML at {} has no <invoiceNumber> element \
                     (file is tampered or empty; depth reached {} before EOF) (S184)",
                    path.display(),
                    depth
                ));
            }
            _ => {}
        }
    }
}

/// S369 — count the `<line>` elements in a base invoice's on-disk NAV
/// XML. The chain emitters offset their CREATE-mode
/// `<lineNumberReference>` values past this count so a storno /
/// modification never reuses a line number NAV already recorded on the
/// base (NAV business rule `INVOICE_LINE_ALREADY_EXISTS`, S370 prod
/// incident). Reading the count from the base's on-disk XML — the
/// canonical record of what NAV holds — mirrors the
/// [`read_invoice_number_from_xml`] S184 discipline: re-deriving from
/// the in-memory base row would drift if the row's line set were edited
/// between base issuance and chain emission.
///
/// Counts `<line>` START events by exact local name (`line`), which is
/// distinct from every sibling element (`lineNumber`, `lineDescription`,
/// `lineNumberReference`, …) so the count is the invoice's line count
/// even though those siblings share the `line` prefix. Tolerates the
/// default-namespace declaration the same way the number reader does.
/// Loud-fails when the file cannot be read or the XML is malformed; a
/// well-formed body with zero `<line>` elements returns `0` (the XSD
/// validator already rejects a zero-line invoice upstream, so a `0`
/// here means a tampered base XML — the caller's chain emit then offsets
/// by 0, which is the conservative no-collision-protection fallback and
/// still flagged loud by NAV if it actually collides).
pub fn count_invoice_lines_from_xml(path: &std::path::Path) -> Result<usize> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "read base NAV XML at {} to count <line> elements for chain line offset (S369)",
            path.display()
        )
    })?;
    let xml = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "base NAV XML at {} is not valid UTF-8 (S369)",
            path.display()
        )
    })?;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut count: usize = 0;
    loop {
        match reader.read_event().with_context(|| {
            format!(
                "parse base NAV XML at {} while counting <line> elements (S369)",
                path.display()
            )
        })? {
            quick_xml::events::Event::Start(e) if e.name().local_name().as_ref() == b"line" => {
                count += 1;
            }
            quick_xml::events::Event::Eof => return Ok(count),
            _ => {}
        }
    }
}

/// S381/F2 — read the base invoice's `<invoiceDeliveryDate>` from its
/// on-disk NAV XML so a storno can copy it rather than stamping the
/// storno's own issue date. NAV's `UNINTENDED_CANCELLATION_DELIVERY_DATE`
/// WARN (Annex I, ID 11401) fires whenever a cancellation moves the
/// original invoice's delivery date to the cancelling invoice's issue
/// date — the spec literally names this "a common error in invoicing
/// programs". Beyond the WARN, the delivery date drives VAT-period
/// assignment, so a divergent storno date asserts the reversal in the
/// wrong period on the regulatory record.
///
/// Returns the canonical `YYYY-MM-DD` string verbatim (same on-disk
/// canonical-record discipline as [`read_invoice_number_from_xml`] —
/// re-deriving from the in-memory base row would drift). Matches the
/// FIRST `<invoiceDeliveryDate>` (the base body carries exactly one).
/// Loud-fails when the file cannot be read, the XML is malformed, or no
/// `<invoiceDeliveryDate>` element appears (a tampered/foreign base XML).
pub fn read_invoice_delivery_date_from_xml(path: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "read base NAV XML at {} to extract <invoiceDeliveryDate> for storno (S381/F2)",
            path.display()
        )
    })?;
    let xml = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "base NAV XML at {} is not valid UTF-8 (S381/F2)",
            path.display()
        )
    })?;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event().with_context(|| {
            format!(
                "parse base NAV XML at {} while seeking <invoiceDeliveryDate> (S381/F2)",
                path.display()
            )
        })? {
            quick_xml::events::Event::Start(e)
                if e.name().local_name().as_ref() == b"invoiceDeliveryDate" =>
            {
                let text = reader
                    .read_text(e.to_end().name())
                    .with_context(|| {
                        format!(
                            "read text of <invoiceDeliveryDate> in base NAV XML at {} (S381/F2)",
                            path.display()
                        )
                    })?
                    .into_owned();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Err(anyhow!(
                        "base NAV XML at {} has an empty <invoiceDeliveryDate> element (S381/F2)",
                        path.display()
                    ));
                }
                return Ok(trimmed.to_string());
            }
            quick_xml::events::Event::Eof => {
                return Err(anyhow!(
                    "base NAV XML at {} has no <invoiceDeliveryDate> element \
                     (file is tampered, empty, or not a NAV InvoiceData body) (S381/F2)",
                    path.display()
                ));
            }
            _ => {}
        }
    }
}

/// S390/D — read the base invoice's `<paymentDate>` from its on-disk NAV
/// XML so a storno can copy it rather than stamping the storno's own
/// issue date. Parallel to [`read_invoice_delivery_date_from_xml`]
/// (S381/F2): a storno that moves the payment date to the cancelling
/// invoice's issue date asserts a payment deadline NAV never saw on the
/// base. Copying the base's `<paymentDate>` keeps the storno's
/// `<invoiceDetail>` a faithful mirror of the base record (the storno is
/// a 1:1 reversal — every detail field NAV holds on the base should
/// reappear unchanged on the cancellation).
///
/// Returns the canonical `YYYY-MM-DD` string verbatim (same on-disk
/// canonical-record discipline as [`read_invoice_number_from_xml`]).
/// Matches the FIRST `<paymentDate>` (the base body carries exactly
/// one). Loud-fails when the file cannot be read, the XML is malformed,
/// or no `<paymentDate>` element appears (a tampered/foreign base XML).
///
/// A separate parallel function (NOT a generic dates reader) per the
/// brief's conservative choice — the two callers read independently and
/// fold each into its own `with_context` chain, so coupling them into
/// one multi-return parser would buy nothing but a wider blast radius.
pub fn read_invoice_payment_date_from_xml(path: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "read base NAV XML at {} to extract <paymentDate> for storno (S390/D)",
            path.display()
        )
    })?;
    let xml = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "base NAV XML at {} is not valid UTF-8 (S390/D)",
            path.display()
        )
    })?;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event().with_context(|| {
            format!(
                "parse base NAV XML at {} while seeking <paymentDate> (S390/D)",
                path.display()
            )
        })? {
            quick_xml::events::Event::Start(e)
                if e.name().local_name().as_ref() == b"paymentDate" =>
            {
                let text = reader
                    .read_text(e.to_end().name())
                    .with_context(|| {
                        format!(
                            "read text of <paymentDate> in base NAV XML at {} (S390/D)",
                            path.display()
                        )
                    })?
                    .into_owned();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Err(anyhow!(
                        "base NAV XML at {} has an empty <paymentDate> element (S390/D)",
                        path.display()
                    ));
                }
                return Ok(trimmed.to_string());
            }
            quick_xml::events::Event::Eof => {
                return Err(anyhow!(
                    "base NAV XML at {} has no <paymentDate> element \
                     (file is tampered, empty, or not a NAV InvoiceData body) (S390/D)",
                    path.display()
                ));
            }
            _ => {}
        }
    }
}

/// S384/F5 — reconstruct the `LineItem`s from an on-disk NAV
/// `<InvoiceData>` body so a STORNO can reverse the lines of every SAVED
/// prior modification in the chain (not just the base's).
///
/// Why this is needed: a chain modification is emitted as a full-replace
/// body whose lines are CREATE operations continuing PAST the base's
/// line numbers (S369). In NAV's data warehouse each chain body's lines
/// are fresh fact rows (see `CHAIN_LINE_OPERATION`), so after `base +
/// MODIFY` NAV holds base lines AND the modification's lines. A storno
/// that reverses only the base leaves the modification's net un-zeroed →
/// NAV's `INCONSISTENT_MODIFICATION_DATA_*_NOT_ZERO*` WARN class. The
/// storno must therefore reverse base + every SAVED modification's
/// lines; `issue_storno` folds this function's output across the chain.
///
/// Round-trips the [`write_lines`] emit exactly: the reconstructed
/// `unit_price` / `quantity` / `vat_rate_basis_points` re-derive the
/// same `net_total` / `vat_amount` / `gross_total` the original body
/// carried (the emit recomputes them from these three fields), so a
/// `negate_line` over the result yields a byte-exact reversal.
///
/// Field reconstruction per `<line>`:
/// - `description` ← `<lineDescription>`
/// - `quantity` ← `<quantity>` (decimal)
/// - `unit_price` ← `<unitPrice>` — minor units inferred from format:
///   a value with a `.` is two-decimal native cents (EUR) → `×100`; an
///   integer is whole forints (HUF). This is unambiguous because
///   `format_native_amount` emits a `.` iff the currency is non-HUF.
/// - `vat_rate_basis_points` ← `<lineVatRate><vatPercentage>` (a NAV
///   fraction, `0.27` = 27%) `×10000`.
/// - `unit` ← `<unitOfMeasure>` (+ `<unitOfMeasureOwn>` when `OWN`).
/// - `note` ← always `None` — per-line notes never reach the NAV XML
///   (ADR-0042 never-leak invariant), and the reversal does not need
///   them.
///
/// Loud-fails (CLAUDE.md #12) on unreadable file, malformed XML, or a
/// `<line>` missing any amount-bearing field — a silent default would
/// ship a wrong reversal quantity that NAV cannot detect.
pub fn read_invoice_lines_from_xml(path: &std::path::Path) -> Result<Vec<LineItem>> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "read chain-member NAV XML at {} to extract <line> items for storno reversal (S384/F5)",
            path.display()
        )
    })?;
    let xml = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "chain-member NAV XML at {} is not valid UTF-8 (S384/F5)",
            path.display()
        )
    })?;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out: Vec<LineItem> = Vec::new();
    let mut cur: Option<XmlLineAcc> = None;

    loop {
        let ev = reader.read_event().with_context(|| {
            format!(
                "parse chain-member NAV XML at {} while extracting <line> items (S384/F5)",
                path.display()
            )
        })?;
        match ev {
            Event::Start(e) if e.name().local_name().as_ref() == b"line" => {
                cur = Some(XmlLineAcc::default());
            }
            Event::Start(e) if cur.is_some() => {
                let local = e.name().local_name();
                let field: Option<&mut Option<String>> = match local.as_ref() {
                    b"lineDescription" => cur.as_mut().map(|a| &mut a.description),
                    b"quantity" => cur.as_mut().map(|a| &mut a.quantity),
                    b"unitOfMeasure" => cur.as_mut().map(|a| &mut a.unit_of_measure),
                    b"unitOfMeasureOwn" => cur.as_mut().map(|a| &mut a.unit_of_measure_own),
                    b"unitPrice" => cur.as_mut().map(|a| &mut a.unit_price),
                    // `<vatPercentage>` appears inside `<lineVatRate>`
                    // only while we are inside a `<line>` (the summary
                    // `<vatRate>` block is outside any `<line>`), so
                    // capturing it unconditionally here is safe.
                    b"vatPercentage" => cur.as_mut().map(|a| &mut a.vat_percentage),
                    _ => None,
                };
                if let Some(slot) = field {
                    let end = e.to_end().into_owned();
                    let text = reader
                        .read_text(end.name())
                        .with_context(|| {
                            format!(
                                "read text of <{}> in chain-member NAV XML at {} (S384/F5)",
                                String::from_utf8_lossy(local.as_ref()),
                                path.display()
                            )
                        })?
                        .into_owned();
                    *slot = Some(text.trim().to_string());
                }
            }
            Event::End(e) if e.name().local_name().as_ref() == b"line" => {
                let acc = cur.take().ok_or_else(|| {
                    anyhow!(
                        "</line> without a matching <line> in {} (S384/F5)",
                        path.display()
                    )
                })?;
                out.push(line_item_from_acc(&acc, path)?);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(out)
}

/// Per-`<line>` leaf-string accumulator for [`read_invoice_lines_from_xml`].
/// Fields fill as their leaf elements are seen between `<line>` and
/// `</line>`; [`line_item_from_acc`] then validates + converts them.
#[derive(Default)]
struct XmlLineAcc {
    description: Option<String>,
    quantity: Option<String>,
    unit_of_measure: Option<String>,
    unit_of_measure_own: Option<String>,
    unit_price: Option<String>,
    vat_percentage: Option<String>,
}

/// Build a [`LineItem`] from the leaf strings captured for one `<line>`.
/// Separate fn so [`read_invoice_lines_from_xml`]'s event loop stays
/// readable. Returns a named error per missing/malformed field.
fn line_item_from_acc(acc: &XmlLineAcc, path: &std::path::Path) -> Result<LineItem> {
    let description = acc.description.clone().ok_or_else(|| {
        anyhow!(
            "<line> missing <lineDescription> in {} (S384/F5)",
            path.display()
        )
    })?;

    let quantity_str = acc
        .quantity
        .as_deref()
        .ok_or_else(|| anyhow!("<line> missing <quantity> in {} (S384/F5)", path.display()))?;
    let quantity = Decimal::from_str_exact(quantity_str).with_context(|| {
        format!(
            "parse <quantity> '{quantity_str}' as decimal in {} (S384/F5)",
            path.display()
        )
    })?;

    let unit_price_str = acc
        .unit_price
        .as_deref()
        .ok_or_else(|| anyhow!("<line> missing <unitPrice> in {} (S384/F5)", path.display()))?;
    let unit_price = Huf(parse_native_minor_units(unit_price_str).with_context(|| {
        format!(
            "parse <unitPrice> '{unit_price_str}' as native minor units in {} (S384/F5)",
            path.display()
        )
    })?);

    let vat_str = acc.vat_percentage.as_deref().ok_or_else(|| {
        anyhow!(
            "<line> missing <vatPercentage> in {} (S384/F5)",
            path.display()
        )
    })?;
    let vat_rate_basis_points =
        parse_vat_percentage_to_basis_points(vat_str).with_context(|| {
            format!(
                "parse <vatPercentage> '{vat_str}' to basis points in {} (S384/F5)",
                path.display()
            )
        })?;

    let unit = match acc.unit_of_measure.as_deref() {
        Some("OWN") => acc.unit_of_measure_own.clone().map(ProductUnit::Own),
        Some(token) => NavUnitOfMeasure::from_nav_token(token).map(ProductUnit::Nav),
        None => None,
    };

    Ok(LineItem {
        description,
        quantity,
        unit_price,
        vat_rate_basis_points,
        // ADR-0101 / Session-1 shut door — every invoice on disk is a
        // `Percent` line (preflight rejects every other kind), so this
        // XML-reconstruction path only ever sees `<vatPercentage>` and
        // `Percent` is exact. ⚠ Session-2 FLAG: when preflight opens to the
        // non-`Percent` kinds, a chain-member XML that renders `vatExemption`
        // / `vatOutOfScope` / `vatDomesticReverseCharge` instead of
        // `<vatPercentage>` would loud-fail the `parse_vat_percentage…`
        // step above (no silent mis-categorisation — acceptable, but the
        // storno-of-modification fold must then reconstruct the kind from
        // the choice element here, or replay the side-store `input.json`).
        vat_rate_kind: VatRateKind::Percent,
        // ADR-0042 — per-line notes are never in the NAV XML, so a
        // chain-member reversal carries none.
        note: None,
        unit,
    })
}

/// Parse a `format_native_amount` output back to its i64 minor units.
/// A value containing `.` is two-decimal native cents (EUR branch) →
/// integer-part×100 + fractional (padded to 2). An integer is whole
/// forints (HUF branch). This is the exact inverse of
/// [`format_native_amount`]; the `.` is present iff the source currency
/// was non-HUF, so the format alone disambiguates without a currency arg.
fn parse_native_minor_units(s: &str) -> Result<i64> {
    let s = s.trim();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let magnitude: i64 = if let Some((int_part, frac_part)) = digits.split_once('.') {
        if frac_part.len() > 2 {
            return Err(anyhow!(
                "native amount '{s}' has more than two decimal places (S384/F5)"
            ));
        }
        let int_units: i64 = int_part
            .parse()
            .with_context(|| format!("parse integer part of native amount '{s}'"))?;
        // Pad the fractional part to exactly two digits ("5" → "50").
        let frac_padded = format!("{frac_part:0<2}");
        let frac_units: i64 = frac_padded
            .parse()
            .with_context(|| format!("parse fractional part of native amount '{s}'"))?;
        int_units
            .checked_mul(100)
            .and_then(|v| v.checked_add(frac_units))
            .ok_or_else(|| anyhow!("native amount '{s}' overflows i64 (S384/F5)"))?
    } else {
        digits
            .parse()
            .with_context(|| format!("parse integer native amount '{s}'"))?
    };
    Ok(if neg { -magnitude } else { magnitude })
}

/// Parse NAV's `<vatPercentage>` (a fraction string, `0.27` = 27%) into
/// the `vat_rate_basis_points` integer (2700). Inverse of
/// [`write_line_vat_rate`]'s `bp as f64 / 10000.0` formatting.
fn parse_vat_percentage_to_basis_points(s: &str) -> Result<u16> {
    use rust_decimal::prelude::ToPrimitive;
    let frac = Decimal::from_str_exact(s.trim())
        .with_context(|| format!("parse vatPercentage '{s}' as decimal"))?;
    let bp = (frac * Decimal::from(10000)).round();
    bp.to_u16()
        .ok_or_else(|| anyhow!("vatPercentage '{s}' is out of basis-point range (S384/F5)"))
}

/// Write the rendered XML to a file path **atomically** (S382/F4).
///
/// Writes to a same-directory `.<name>.tmp.<pid>-<nanos>-<seq>`, fsyncs
/// the bytes, then `rename`s onto the final path (a POSIX-atomic,
/// same-filesystem operation) and fsyncs the parent directory so the
/// rename is durable. A machine crash can no longer leave a
/// half-written or page-cache-only NAV XML beside an already-fsync'd DB
/// row — the file at `path` either does not exist or is the complete
/// bytes. Pre-S381 this was a naive `File::create` + `write_all`
/// (truncate-in-place, no fsync), so a crash mid-write recreated the
/// split-state defect S375 closed on the DB side.
///
/// Mirrors the `numbering::write_atomic` / `seller_banks::write_atomic`
/// pattern already used for `seller.toml`. Every NAV-XML emit site goes
/// through this one helper (`issue_invoice`, `issue_storno`,
/// `issue_modification`, `request_technical_annulment`), so the
/// atomicity guarantee lands at all four sites at once.
///
/// S385 — the crash-safe write/fsync/rename body was lifted verbatim
/// into `crate::fs::write_atomic` so the quote-PDF re-render daemon
/// (which had a naive `std::fs::write`) shares the exact same sequence.
/// This function stays as the NAV-XML-named entry point — the four chain
/// emit sites keep calling `write_to_path` unchanged — and forwards to
/// the shared helper.
pub fn write_to_path(path: &std::path::Path, xml: &[u8]) -> Result<()> {
    crate::fs::write_atomic(path, xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hungarian_tax_number_decomposes_canonical_form() {
        let parsed = parse_hungarian_tax_number("24904362-2-41").expect("canonical form");
        assert_eq!(parsed.taxpayer_id, "24904362");
        assert_eq!(parsed.vat_code, "2");
        assert_eq!(parsed.county_code, "41");
    }

    #[test]
    fn parse_hungarian_tax_number_trims_surrounding_whitespace() {
        let parsed = parse_hungarian_tax_number("  12345678-1-42  ").expect("trims");
        assert_eq!(parsed.taxpayer_id, "12345678");
        assert_eq!(parsed.vat_code, "1");
        assert_eq!(parsed.county_code, "42");
    }

    #[test]
    fn parse_hungarian_tax_number_rejects_empty() {
        let err = parse_hungarian_tax_number("").unwrap_err();
        assert!(matches!(err, SupplierConfigError::MissingTaxNumber));
    }

    #[test]
    fn parse_hungarian_tax_number_rejects_bare_eight_digits() {
        // Pre-PR-50 fixtures used `12345678` (bare base) and rode the
        // flat-string renderer; the new shape demands `xxxxxxxx-y-zz`
        // so the bare base is rejected at the boundary.
        let err = parse_hungarian_tax_number("12345678").unwrap_err();
        assert!(matches!(
            err,
            SupplierConfigError::MalformedTaxNumber { .. }
        ));
    }

    #[test]
    fn parse_hungarian_tax_number_rejects_non_digit_segments() {
        let err = parse_hungarian_tax_number("abcd1234-2-41").unwrap_err();
        match err {
            SupplierConfigError::MalformedTaxNumber { reason, .. } => {
                assert!(
                    reason.contains("taxpayerId"),
                    "reason must name the bad segment: {reason}"
                );
            }
            other => panic!("expected MalformedTaxNumber, got {other:?}"),
        }
    }

    #[test]
    fn parse_hungarian_tax_number_rejects_wrong_segment_lengths() {
        // 7 + 1 + 2 — taxpayer too short.
        let err = parse_hungarian_tax_number("1234567-1-42").unwrap_err();
        assert!(matches!(
            err,
            SupplierConfigError::MalformedTaxNumber { .. }
        ));
        // 8 + 2 + 2 — vat-code too long.
        let err = parse_hungarian_tax_number("12345678-12-42").unwrap_err();
        assert!(matches!(
            err,
            SupplierConfigError::MalformedTaxNumber { .. }
        ));
        // 8 + 1 + 3 — county-code too long.
        let err = parse_hungarian_tax_number("12345678-1-421").unwrap_err();
        assert!(matches!(
            err,
            SupplierConfigError::MalformedTaxNumber { .. }
        ));
    }

    #[test]
    fn validate_supplier_info_accepts_valid_dashed_form() {
        let s = SupplierInfo {
            tax_number: "24904362-2-41".to_string(),
            name: "Áben Consulting KFT.".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatérő köz 6".to_string(),
        };
        assert!(validate_supplier_info(&s).is_ok());
    }

    #[test]
    fn validate_supplier_info_rejects_empty_tax_number() {
        let s = SupplierInfo {
            tax_number: String::new(),
            name: "X".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatérő köz 6".to_string(),
        };
        assert!(matches!(
            validate_supplier_info(&s),
            Err(SupplierConfigError::MissingTaxNumber)
        ));
    }

    // ── PR-97 / ADR-0048 — CustomerVatStatus closed-vocab ────────────

    #[test]
    fn customer_vat_status_serde_round_trip_pin() {
        // Each variant must round-trip through serde JSON as its
        // PascalCase literal. Mirrors `partner_kind_serde_round_trip_pin`
        // — a variant rename here that drifts from the SPA's string-
        // union mirror (api.ts::CustomerVatStatusBody) surfaces here
        // first. CLAUDE.md rule 9.
        for (variant, literal) in [
            (CustomerVatStatus::Domestic, "\"Domestic\""),
            (CustomerVatStatus::PrivatePerson, "\"PrivatePerson\""),
            (CustomerVatStatus::Other, "\"Other\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(
                json, literal,
                "CustomerVatStatus::{:?} must emit {}",
                variant, literal
            );
            let back: CustomerVatStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn customer_vat_status_nav_token_is_screaming_snake() {
        // The NAV wire token is SCREAMING_SNAKE_CASE (DOMESTIC /
        // PRIVATE_PERSON / OTHER) — NOT the Rust PascalCase. Pinned
        // verbatim so a future "let's unify the casing" refactor cannot
        // silently break the NAV submit.
        assert_eq!(CustomerVatStatus::Domestic.as_nav_token(), "DOMESTIC");
        assert_eq!(
            CustomerVatStatus::PrivatePerson.as_nav_token(),
            "PRIVATE_PERSON"
        );
        assert_eq!(CustomerVatStatus::Other.as_nav_token(), "OTHER");
    }

    #[test]
    fn customer_vat_status_db_round_trip() {
        // PascalCase storage layer matches the wire mirror.
        for variant in [
            CustomerVatStatus::Domestic,
            CustomerVatStatus::PrivatePerson,
            CustomerVatStatus::Other,
        ] {
            let s = variant.as_db_str();
            assert_eq!(CustomerVatStatus::from_db_str(s), Some(variant));
        }
        assert_eq!(CustomerVatStatus::from_db_str("garbage"), None);
    }

    #[test]
    fn customer_vat_status_default_is_domestic() {
        // Pre-PR-97 wire bodies omit the field — serde defaults to
        // Domestic which matches the pre-PR-97 implicit posture.
        assert_eq!(CustomerVatStatus::default(), CustomerVatStatus::Domestic);
    }

    /// Session-154 (ADR-0048 amendment 2026-05-29) — INVERTS the
    /// session-148 pin. NAV's business-tier rule
    /// `CUSTOMER_DATA_NOT_EXPECTED` ("Magánszemély vevő adatai nem adhatók
    /// meg.") ABORTS a PRIVATE_PERSON submit that carries `<customerName>`
    /// or `<customerAddress>`. Confirmed against NAV's response XML to
    /// Ervin's invoice 31 (2026-05-29). §169 buyer-name/address is a
    /// *printed-invoice* obligation (the PDF, unchanged) — it does NOT
    /// govern the NAV wire. The wire emit therefore SUPPRESSES both fields
    /// for natural-person buyers.
    #[test]
    fn write_customer_omits_customer_name_for_private_person() {
        let c = CustomerInfo {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            // PrivatePerson carries no ADÓSZÁM.
            tax_number: None,
            name: "Teszt Magánszemély".to_string(),
            address: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = Writer::new(&mut buf);
            write_customer(&mut w, &c).expect("write_customer");
        }
        let xml = String::from_utf8(buf).expect("utf8");
        assert!(
            !xml.contains("<customerName>"),
            "PRIVATE_PERSON wire body must OMIT <customerName> \
             (NAV CUSTOMER_DATA_NOT_EXPECTED), got: {xml}"
        );
    }

    /// Session-154 — the wire emit SUPPRESSES `<customerAddress>` for
    /// PRIVATE_PERSON even when an address IS present on the struct (§169
    /// preflight populates it for the PDF). NAV rule
    /// `CUSTOMER_DATA_NOT_EXPECTED` forbids it on the wire regardless.
    /// Strengthens the prior `address: None` pin by proving suppression of
    /// a *populated* address. `<customerVatData>` omission is held too.
    #[test]
    fn write_customer_omits_address_for_private_person_with_address() {
        let c = CustomerInfo {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            name: "Teszt Magánszemély".to_string(),
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1011".to_string(),
                city: "Budapest".to_string(),
                street: "Fő utca 1.".to_string(),
            }),
        };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = Writer::new(&mut buf);
            write_customer(&mut w, &c).expect("write_customer");
        }
        let xml = String::from_utf8(buf).expect("utf8");
        assert!(
            !xml.contains("<customerAddress>"),
            "PRIVATE_PERSON must OMIT <customerAddress> on the wire even when \
             populated (NAV CUSTOMER_DATA_NOT_EXPECTED), got: {xml}"
        );
        assert!(
            !xml.contains("<customerVatData>"),
            "PRIVATE_PERSON must omit <customerVatData> on the wire, got: {xml}"
        );
    }

    /// Session-154 — positive regression: DOMESTIC buyers still emit
    /// `<customerName>` AND `<customerAddress>` on the wire (PR-148/150
    /// path for non-natural-person buyers is unchanged). Guards against an
    /// over-broad suppression that would strip these from taxable buyers.
    #[test]
    fn write_customer_emits_name_and_address_for_domestic() {
        let c = CustomerInfo {
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("24904362-2-41".to_string()),
            name: "Teszt Kft.".to_string(),
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1011".to_string(),
                city: "Budapest".to_string(),
                street: "Fő utca 1.".to_string(),
            }),
        };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = Writer::new(&mut buf);
            write_customer(&mut w, &c).expect("write_customer");
        }
        let xml = String::from_utf8(buf).expect("utf8");
        assert!(
            xml.contains("<customerName>Teszt Kft.</customerName>"),
            "DOMESTIC wire body must carry <customerName>, got: {xml}"
        );
        assert!(
            xml.contains("<customerAddress>"),
            "DOMESTIC wire body must carry <customerAddress>, got: {xml}"
        );
    }

    #[test]
    fn supplier_config_error_display_names_the_input() {
        let err = SupplierConfigError::MalformedTaxNumber {
            input: "24904362".to_string(),
            reason: "expected three dash-separated segments",
        };
        let msg = err.to_string();
        assert!(msg.contains("24904362"), "must echo the bad input: {msg}");
        assert!(
            msg.contains("xxxxxxxx-y-zz"),
            "must show the expected shape: {msg}"
        );
    }

    /// S369 — `count_invoice_lines_from_xml` counts `<line>` elements
    /// (NOT the `lineNumber` / `lineNumberReference` siblings that share
    /// the `line` prefix), tolerating the default-namespace declaration.
    /// This is the prod read path the chain emitters use to compute the
    /// CREATE-line offset that avoids NAV's INVOICE_LINE_ALREADY_EXISTS.
    #[test]
    fn count_invoice_lines_from_xml_counts_line_elements_only() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<InvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data">
  <invoiceMain><invoice><invoiceLines>
    <line>
      <lineNumber>1</lineNumber>
      <lineModificationReference><lineNumberReference>1</lineNumberReference></lineModificationReference>
    </line>
    <line><lineNumber>2</lineNumber></line>
    <line><lineNumber>3</lineNumber></line>
  </invoiceLines></invoice></invoiceMain>
</InvoiceData>"#;
        let path =
            std::env::temp_dir().join(format!("aberp_s369_count_lines_{}.xml", std::process::id()));
        std::fs::write(&path, xml).expect("write temp base XML");
        let count = count_invoice_lines_from_xml(&path).expect("count lines");
        // Three `<line>` elements — the four `lineNumber` / one
        // `lineNumberReference` siblings must NOT inflate the count.
        assert_eq!(count, 3, "must count only the three <line> elements");
        let _ = std::fs::remove_file(&path);
    }

    /// S369 — a missing base XML file loud-fails (the chain emitter must
    /// not silently offset by 0 because it could not read the base).
    #[test]
    fn count_invoice_lines_from_xml_loud_fails_on_missing_file() {
        let path = std::env::temp_dir().join("aberp_s369_count_lines_missing_NOPE.xml");
        let _ = std::fs::remove_file(&path);
        let err = count_invoice_lines_from_xml(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("read base NAV XML"),
            "must name the read failure: {msg}"
        );
    }
}
