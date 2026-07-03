//! S196 / PR-196 — partner + product catalog extraction from
//! NAV-restored invoice XMLs (the v2 follow-on to S180's digest-only
//! restore wizard).
//!
//! # What this module owns
//!
//! S180 ([`crate::restore_from_nav_outgoing`]) ships **digest-only**:
//! `queryInvoiceDigest OUTBOUND` is paginated month-by-month and every
//! new digest mints a `restored_invoice` row plus an
//! `InvoiceRestoredFromNav` audit entry. The digest carries totals +
//! issue date + currency — **but no customer block and no line items**.
//! S196 closes the catalog gap: for every digest that's freshly
//! restored in the current cycle, the wizard now also issues one
//! `queryInvoiceData` call to fetch the inner `<InvoiceData>` XML (the
//! base64-encoded original submission body NAV holds), parses the
//! `<customerInfo>` block + `<invoiceLines>` block, and dedupes those
//! candidates into the local `partners` + `products` catalogs.
//!
//! # Scope boundary (v2 ↔ v3)
//!
//!   - **Fresh-restored-only.** Catalog extraction fires when
//!     [`crate::restore_from_nav_outgoing::process_digest`] returns
//!     `Restored`. A cache-hit digest (Skipped — restored in a prior
//!     cycle) does NOT re-fetch its `queryInvoiceData` body. The
//!     v1 → v2 upgrade path for tenants that already populated
//!     `restored_invoice` from S180 is named-deferred to v3 (the
//!     operator can wipe the year's `restored_invoice` rows and
//!     re-run to backfill; an in-place backfill route is a v3
//!     addition).
//!   - **No new audit kind.** The brief invited extending the
//!     existing `InvoiceRestoredFromNav` payload OR adding new
//!     `PartnerRestoredFromNav` / `ProductRestoredFromNav` kinds.
//!     Neither lands: the catalog rows themselves are durable
//!     evidence (with row-level `created_at` provenance), the cycle's
//!     counts ride the HTTP response body, and per-invoice extraction
//!     failures surface loud via `tracing::warn!` per CLAUDE.md
//!     rule 12. CLAUDE.md rule 13 — the deletion is the cleanest fit.
//!   - **No `restored_invoice` schema bump.** No
//!     `catalog_extracted_at` column. The fresh-restored-only posture
//!     means we never need to ask "has extraction happened for this
//!     row?" — by construction it has, if it could.
//!
//! # Dedupe keys
//!
//!   - **Partners — DOMESTIC.** Lookup by canonical `tax_number`
//!     (digits + `-` already byte-exact from
//!     `<customerTaxNumber>/<taxpayerId>`/`<vatCode>`/`<countyCode>`
//!     re-composition). Reuses
//!     [`crate::partners::find_partner_by_tax_number`] (extant since
//!     PR-92). Hit → skip. Miss → `create_partner` with kind=Customer.
//!   - **Partners — PRIVATE_PERSON.** Lookup by `(legal_name,
//!     address_country, address_postal_code, address_city,
//!     address_street)` tuple via [`crate::partners::find_partner_by_name_and_address`]
//!     (S196 add). Each component canonicalised (trim + lowercase)
//!     before the lookup. Strict equality — no fuzzy match (the brief
//!     names the duplicate-cleanup pass as an operator-driven follow-up).
//!     A PRIVATE_PERSON candidate with empty/missing `customerName`
//!     surfaces a `tracing::warn!` and skips partner insertion for
//!     that invoice (the dedup key collapses to nothing). The invoice
//!     itself is already restored; only its partner-extraction fails.
//!   - **Products.** Lookup by `(name, ProductUnit)` tuple via
//!     [`crate::products::find_product_by_name_and_unit`] (S196 add).
//!     Name case-insensitive after trim; `ProductUnit` byte-exact on
//!     the two-column DB form (`Nav("PIECE")` vs `Own("liter@15C")`
//!     stay distinct rows). Hit + same `unit_price_minor` → skip.
//!     Hit + different price → UPDATE the price (last-seen wins, per
//!     the brief's `products_price_varies` semantic) + WARN. Miss →
//!     `create_product`.
//!
//! # NAV `queryInvoiceData` shape
//!
//! `queryInvoiceData` returns a `<QueryInvoiceDataResponse>` whose
//! `<invoiceDataResult>/<invoiceData>` element is a base64-encoded
//! blob — the verbatim bytes the supplier originally submitted via
//! `manageInvoice` (the inner `<InvoiceData>` root from the NAV v3.0
//! invoice XSD). Decoded, it's a full XML document carrying
//! `<invoiceMain>/<invoice>/<invoiceHead>/<customerInfo>` and the
//! sibling `<invoiceLines>`. The extractor parses the decoded inner
//! XML directly; the outer envelope is consumed only for the base64
//! payload by [`extract_inner_invoice_data_xml`].
//!
//! # Error posture
//!
//! Every per-invoice failure (queryInvoiceData transport / NAV ERROR /
//! malformed inner XML / partner-or-product insert error) is contained
//! within the per-invoice `ExtractionDelta` and surfaced via
//! `tracing::warn!`. The wizard's overall pipeline does NOT abort on a
//! single extraction failure (the invoice restore itself already
//! succeeded — the wizard's primary contract is fulfilled). The
//! cycle-level totals carried back to the operator (`*_errored`
//! counters) make the silent-skip risk loud per CLAUDE.md rule 12.

use std::path::Path;

use aberp_billing::Currency;
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use duckdb::Connection;
use quick_xml::events::Event;
use quick_xml::Reader;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use crate::nav_xml::CustomerVatStatus;
use crate::partners::{self, PartnerInputs, PartnerKind};
use crate::products::{self, NavUnitOfMeasure, ProductInputs, ProductUnit};

// ──────────────────────────────────────────────────────────────────────
// Counters surface.
// ──────────────────────────────────────────────────────────────────────

/// Counters from a single invoice's catalog-extraction pass. The
/// wizard's `walk_month` accumulates these via [`Self::add`] and the
/// totals ride the HTTP response body as `RestoreSummary` fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractionDelta {
    /// Partners freshly inserted into `partners` this invoice.
    /// At most 1 (each invoice carries exactly one customer block);
    /// 0 when the partner dedupe key hit an existing row.
    pub partners_restored: u64,
    /// Partner candidates that matched an existing row by the dedup
    /// key (`tax_number` for DOMESTIC, `(name, address)` for
    /// PRIVATE_PERSON). At most 1.
    pub partners_skipped_duplicate: u64,
    /// Partner extraction failed (parser error, missing required
    /// fields, DB error). The invoice itself is already restored —
    /// this counter only reflects the partner-extraction sub-step.
    pub partners_errored: u64,
    /// Products freshly inserted into `products` this invoice.
    pub products_restored: u64,
    /// Product candidates that matched an existing row (same name +
    /// unit + price).
    pub products_skipped_duplicate: u64,
    /// Per-line product extraction failed.
    pub products_errored: u64,
    /// Subset of "products seen" where the candidate's price differed
    /// from the already-stored row's price. The price IS updated (last-
    /// seen wins) — this counter surfaces the price drift so the
    /// operator can audit. Brief calls this a v3 polish target (a
    /// per-product `price_varies: bool` column) — for v2 the count
    /// rides the response + a `tracing::warn!` is emitted per drift.
    pub products_price_varies: u64,
    /// `queryInvoiceData` call failed entirely for this invoice — no
    /// candidates were even parseable. At most 1.
    pub invoice_extraction_errored: u64,
}

impl ExtractionDelta {
    /// Accumulate `other` into `self` in place.
    pub fn add(&mut self, other: ExtractionDelta) {
        self.partners_restored += other.partners_restored;
        self.partners_skipped_duplicate += other.partners_skipped_duplicate;
        self.partners_errored += other.partners_errored;
        self.products_restored += other.products_restored;
        self.products_skipped_duplicate += other.products_skipped_duplicate;
        self.products_errored += other.products_errored;
        self.products_price_varies += other.products_price_varies;
        self.invoice_extraction_errored += other.invoice_extraction_errored;
    }
}

// ──────────────────────────────────────────────────────────────────────
// Candidate structs — parsed-from-XML representations.
// ──────────────────────────────────────────────────────────────────────

/// One partner candidate extracted from `<invoiceHead>/<customerInfo>`.
///
/// `tax_number` is `Some(_)` exactly when `vat_status` is
/// `Domestic` — the NAV XSD enforces the constraint at the wire layer
/// and the parser surfaces the same shape here. `name` is `Some(_)`
/// whenever the wire body emitted `<customerName>` (DOMESTIC and OTHER
/// always emit it; PRIVATE_PERSON omitted it post-session-154 but
/// older invoices may carry it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerCandidate {
    pub vat_status: CustomerVatStatus,
    pub tax_number: Option<String>,
    pub name: Option<String>,
    pub address_country: Option<String>,
    pub address_postal_code: Option<String>,
    pub address_city: Option<String>,
    pub address_street: Option<String>,
}

/// One line candidate extracted from `<invoiceLines>/<line>`. The
/// description, unit, and price round-trip cleanly back into the
/// `products` table via [`apply_candidates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineCandidate {
    pub description: String,
    pub unit: ProductUnit,
    pub unit_price_minor: i64,
}

// ──────────────────────────────────────────────────────────────────────
// `queryInvoiceData` envelope unwrap.
// ──────────────────────────────────────────────────────────────────────

/// Extract + base64-decode the inner `<invoiceData>` blob from a
/// verbatim `<QueryInvoiceDataResponse>` body. The decoded bytes are
/// the full `<InvoiceData>` XML root the supplier originally submitted
/// via `manageInvoice`.
///
/// Return-shape contract — three outcomes, per PR-215 / S217:
///
///   - `Ok(Some(bytes))` — `<invoiceData>` element present, base64 decoded.
///   - `Ok(None)`        — `<invoiceData>` element ABSENT. Per the official
///                         NAV OSA 3.0 XSD,
///                         `QueryInvoiceDataResponseType.invoiceDataResult`
///                         is `minOccurs=0`; for `queryInvoiceData INBOUND`
///                         NAV legitimately returns `funcCode=OK` without
///                         it when the supplier has not exposed the
///                         original XML payload to the buyer (paper
///                         invoices, partial-data submissions, supplier
///                         opted out of XML republication). The buyer's
///                         entitlement ends at the digest in those cases.
///                         NOT a bug — observed on 13/13 of the 2026-06-01
///                         production cycle's INBOUND rows. Callers MUST
///                         distinguish this from `Err(...)`.
///   - `Err(...)`        — `<invoiceData>` present but empty / base64
///                         garbage. Stays loud per CLAUDE.md rule 12.
///
/// Pre-PR-215 this returned `Result<Vec<u8>>` and loud-failed on the
/// absent case. The OUTBOUND caller is unaffected (a missing
/// `<invoiceData>` for the seller's own invoice is still anomalous —
/// `restore_from_nav_outgoing.rs::extract_catalog_for_invoice` continues
/// to `warn!` + skip on `Ok(None)`). The INBOUND caller in
/// `ap_sync.rs::persist_xml_for_row` treats `Ok(None)` as expected NAV
/// behavior: `info!` + leave `nav_xml_path` NULL + skip `.failed/`
/// diagnostic capture.
pub fn extract_inner_invoice_data_xml(response_xml: &[u8]) -> Result<Option<Vec<u8>>> {
    let b64 = match find_first_text(response_xml, "invoiceData")? {
        Some(text) => text,
        None => return Ok(None),
    };
    let trimmed = b64.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "queryInvoiceData response carries empty <invoiceData> base64 blob — \
             defence-in-depth loud-fail per CLAUDE.md rule 12"
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|e| anyhow!("base64-decode <invoiceData> blob: {e}"))?;
    Ok(Some(bytes))
}

// ──────────────────────────────────────────────────────────────────────
// Inner XML parsers.
// ──────────────────────────────────────────────────────────────────────

/// Parse `<invoiceHead>/<customerInfo>` out of the inner InvoiceData
/// XML. Returns a [`CustomerCandidate`] with the fields the wire
/// carried; missing optional sub-elements stay `None`. Required
/// `<customerVatStatus>` loud-fails if absent or outside the closed
/// vocab.
pub fn parse_customer_info(inner_xml: &[u8]) -> Result<CustomerCandidate> {
    // Scope the search to `<customerInfo>` so a stray `<customerName>`
    // elsewhere (shouldn't exist, but defence-in-depth) cannot leak in.
    let block = extract_element_block(inner_xml, "customerInfo")?.ok_or_else(|| {
        anyhow!("inner InvoiceData XML missing <customerInfo> block — cannot extract partner")
    })?;

    let vat_status_raw = find_first_text(&block, "customerVatStatus")?
        .ok_or_else(|| anyhow!("<customerInfo> missing <customerVatStatus>"))?;
    let vat_status = match vat_status_raw.trim() {
        "DOMESTIC" => CustomerVatStatus::Domestic,
        "PRIVATE_PERSON" => CustomerVatStatus::PrivatePerson,
        "OTHER" => CustomerVatStatus::Other,
        other => {
            return Err(anyhow!(
                "<customerVatStatus> carries `{}` outside the NAV v3.0 closed vocab",
                other
            ));
        }
    };

    let tax_number = match vat_status {
        CustomerVatStatus::Domestic => {
            // Re-compose `xxxxxxxx-y-zz` from the three structured
            // children NAV's XSD enforces under DOMESTIC. Per PR-50:
            // <customerTaxNumber>
            //   <taxpayerId>12345678</taxpayerId>
            //   <vatCode>1</vatCode>
            //   <countyCode>42</countyCode>
            // </customerTaxNumber>
            let inner = extract_element_block(&block, "customerTaxNumber")?.ok_or_else(|| {
                anyhow!("DOMESTIC <customerInfo> missing <customerTaxNumber> structured block")
            })?;
            let taxpayer_id = find_first_text(&inner, "taxpayerId")?
                .ok_or_else(|| anyhow!("<customerTaxNumber> missing <taxpayerId>"))?;
            let vat_code = find_first_text(&inner, "vatCode")?
                .ok_or_else(|| anyhow!("<customerTaxNumber> missing <vatCode>"))?;
            let county_code = find_first_text(&inner, "countyCode")?
                .ok_or_else(|| anyhow!("<customerTaxNumber> missing <countyCode>"))?;
            Some(format!(
                "{}-{}-{}",
                taxpayer_id.trim(),
                vat_code.trim(),
                county_code.trim()
            ))
        }
        // PrivatePerson / Other never carry <customerVatData> on the
        // wire per NAV's CUSTOMER_DATA_EXPECTED business rule.
        _ => None,
    };

    let name = find_first_text(&block, "customerName")?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // `<customerAddress>/<common:simpleAddress>` is the wire shape.
    // We accept either the prefixed or local-name <simpleAddress> via
    // `find_first_text`'s namespace-blind match.
    let (address_country, address_postal_code, address_city, address_street) =
        match extract_element_block(&block, "customerAddress")? {
            Some(addr) => {
                let country = find_first_text(&addr, "countryCode")?
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let postal = find_first_text(&addr, "postalCode")?
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let city = find_first_text(&addr, "city")?
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let street = find_first_text(&addr, "additionalAddressDetail")?
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                (country, postal, city, street)
            }
            None => (None, None, None, None),
        };

    Ok(CustomerCandidate {
        vat_status,
        tax_number,
        name,
        address_country,
        address_postal_code,
        address_city,
        address_street,
    })
}

/// Parse `<invoiceLines>/<line>` out of the inner InvoiceData XML.
/// `currency` is required to convert each `<unitPrice>` (decimal-as-
/// string on the wire) to minor units — HUF rounds to whole forints,
/// EUR rounds to cents. A `<line>` missing required sub-elements
/// surfaces a per-line error to the caller (which aggregates into
/// `products_errored`); other lines on the same invoice still parse.
pub fn parse_invoice_lines(inner_xml: &[u8], currency: Currency) -> Result<Vec<LineCandidate>> {
    let lines_block = extract_element_block(inner_xml, "invoiceLines")?.ok_or_else(|| {
        anyhow!("inner InvoiceData XML missing <invoiceLines> block — cannot extract products")
    })?;
    let mut out = Vec::new();
    let mut cursor = &lines_block[..];
    while let Some(line_block) = extract_element_block(cursor, "line")? {
        let line_pos = locate_element_end(cursor, "line")?
            .ok_or_else(|| anyhow!("internal: extracted <line> block but cannot relocate end"))?;
        let description = find_first_text(&line_block, "lineDescription")?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("<line> missing <lineDescription>"))?;
        let unit_token = find_first_text(&line_block, "unitOfMeasure")?
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "PIECE".to_string());
        let unit = if unit_token == "OWN" {
            let own_label = find_first_text(&line_block, "unitOfMeasureOwn")?
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "<line> emits <unitOfMeasure>OWN</unitOfMeasure> \
                         without <unitOfMeasureOwn> companion (NAV XSD violation)"
                    )
                })?;
            ProductUnit::Own(own_label)
        } else {
            match NavUnitOfMeasure::from_nav_token(&unit_token) {
                Some(t) => ProductUnit::Nav(t),
                None => {
                    return Err(anyhow!(
                        "<line> <unitOfMeasure> `{}` is not a NAV v3.0 closed-vocab token",
                        unit_token
                    ));
                }
            }
        };
        let unit_price_raw = find_first_text(&line_block, "unitPrice")?
            .ok_or_else(|| anyhow!("<line> missing <unitPrice>"))?;
        let unit_price_minor = decimal_to_minor(unit_price_raw.trim(), currency)
            .with_context(|| format!("parse <unitPrice> `{}`", unit_price_raw))?;
        out.push(LineCandidate {
            description,
            unit,
            unit_price_minor,
        });
        cursor = &cursor[line_pos..];
    }
    Ok(out)
}

// ──────────────────────────────────────────────────────────────────────
// Apply parsed candidates to the local catalog tables.
// ──────────────────────────────────────────────────────────────────────

/// Insert/upsert the customer + each line's product into the local
/// `partners` and `products` tables. Returns an [`ExtractionDelta`]
/// the caller accumulates into the wizard-level totals. Per-failure
/// counters increment and surface via `tracing::warn!` — the function
/// does NOT propagate per-candidate errors so a single bad line cannot
/// abort the whole invoice's extraction.
///
/// PR-216 / S218 — the parsed `customer` block is also written back
/// IN-ROW onto `restored_invoice` via
/// [`crate::restore_from_nav_outgoing::update_buyer_fields`] so the
/// SPA outgoing list's Partner column populates without a JOIN to
/// `partners`. The buyer-snapshot write is independent of the
/// partner-master upsert: even if the partner master row already
/// exists (DOMESTIC tax-number hit) we still mirror the name onto the
/// restored row. UPDATE failures here surface a `tracing::warn!` and
/// leave the row's buyer columns NULL — the boot-time backfill
/// ([`crate::restore_from_nav_outgoing::run_buyer_backfill_once`]) will
/// retry on the next launch.
pub fn apply_candidates(
    conn: &Connection,
    tenant: &str,
    invoice_number: &str,
    customer: &CustomerCandidate,
    lines: &[LineCandidate],
    currency: Currency,
) -> ExtractionDelta {
    let mut delta = ExtractionDelta::default();

    // PR-216 / S218 — write the buyer snapshot to `restored_invoice`
    // FIRST, before the partner-master upsert. The two are independent
    // (this UPDATE only touches the restored row; the partner-master
    // upsert touches `partners`). Per the docstring the UPDATE
    // failure path is warn-and-continue.
    if let Err(e) = crate::restore_from_nav_outgoing::update_buyer_fields(
        conn,
        tenant,
        invoice_number,
        customer.name.as_deref(),
        customer.tax_number.as_deref(),
        Some(customer.vat_status.as_db_str()),
    ) {
        tracing::warn!(
            invoice_number = invoice_number,
            error = ?e,
            "S218: restored_invoice buyer-snapshot UPDATE failed; row stays NULL — \
             boot-time backfill will retry"
        );
    }

    match upsert_partner(conn, tenant, customer) {
        Ok(PartnerUpsertOutcome::Inserted) => delta.partners_restored = 1,
        Ok(PartnerUpsertOutcome::SkippedDuplicate) => delta.partners_skipped_duplicate = 1,
        Err(e) => {
            tracing::warn!(
                invoice_number = invoice_number,
                error = ?e,
                "S196: partner extraction failed; the invoice is restored but its \
                 customer block did not yield a partner row — operator may need to \
                 add this partner manually"
            );
            delta.partners_errored = 1;
        }
    }

    for (line_idx, line) in lines.iter().enumerate() {
        match upsert_product(conn, tenant, line, currency) {
            Ok(ProductUpsertOutcome::Inserted) => delta.products_restored += 1,
            Ok(ProductUpsertOutcome::SkippedSameKey) => delta.products_skipped_duplicate += 1,
            Ok(ProductUpsertOutcome::UpdatedPriceVaries) => {
                // Counted as "skipped duplicate" for the headline number
                // (no NEW product row), but the price-drift sub-counter
                // surfaces that we mutated an existing row's price.
                delta.products_skipped_duplicate += 1;
                delta.products_price_varies += 1;
            }
            Err(e) => {
                tracing::warn!(
                    invoice_number = invoice_number,
                    line = line_idx + 1,
                    error = ?e,
                    "S196: product extraction failed for one line; other lines on the \
                     same invoice are unaffected"
                );
                delta.products_errored += 1;
            }
        }
    }

    delta
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartnerUpsertOutcome {
    Inserted,
    SkippedDuplicate,
}

fn upsert_partner(
    conn: &Connection,
    tenant: &str,
    customer: &CustomerCandidate,
) -> Result<PartnerUpsertOutcome> {
    // Dedup key depends on vat status. DOMESTIC keys on tax_number;
    // PRIVATE_PERSON keys on (name, address-tuple). OTHER is not yet
    // a valid partner status (ADR-0048 §7 named-deferred) — we surface
    // a loud error so the operator sees the explicit gap rather than a
    // silently misrouted insert.
    match customer.vat_status {
        CustomerVatStatus::Domestic => {
            let tax_number = customer
                .tax_number
                .as_deref()
                .ok_or_else(|| anyhow!("DOMESTIC customer candidate missing tax_number"))?;
            if partners::find_partner_by_tax_number(conn, tenant, tax_number)?.is_some() {
                return Ok(PartnerUpsertOutcome::SkippedDuplicate);
            }
            let inputs = customer_to_partner_inputs(customer)?;
            partners::create_partner(conn, tenant, &inputs)
                .context("create_partner from NAV-restored DOMESTIC candidate")?;
            Ok(PartnerUpsertOutcome::Inserted)
        }
        CustomerVatStatus::PrivatePerson => {
            // PRIVATE_PERSON dedup needs a name; without one we cannot
            // form a meaningful key. NAV's post-session-154 wire shape
            // suppresses `<customerName>` for PRIVATE_PERSON outright,
            // so older invoices (which DID emit a name) extract cleanly
            // and newer invoices surface this as a per-invoice WARN.
            let legal_name = customer
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "PRIVATE_PERSON customer candidate has no <customerName>; \
                         post-session-154 NAV wire bodies suppress this field on \
                         PRIVATE_PERSON — partner extraction is a no-op for this invoice"
                    )
                })?;
            if partners::find_partner_by_name_and_address(
                conn,
                tenant,
                legal_name,
                customer.address_country.as_deref(),
                customer.address_postal_code.as_deref(),
                customer.address_city.as_deref(),
                customer.address_street.as_deref(),
            )?
            .is_some()
            {
                return Ok(PartnerUpsertOutcome::SkippedDuplicate);
            }
            let inputs = customer_to_partner_inputs(customer)?;
            partners::create_partner(conn, tenant, &inputs)
                .context("create_partner from NAV-restored PRIVATE_PERSON candidate")?;
            Ok(PartnerUpsertOutcome::Inserted)
        }
        CustomerVatStatus::Other => Err(anyhow!(
            "OTHER customer status is ADR-0048 §7 named-deferred; the restore-from-NAV \
             extractor cannot mint an Other-kind partner until v2 of the partner module \
             lands the community/third-state VAT shape"
        )),
    }
}

/// Build a [`PartnerInputs`] from a [`CustomerCandidate`]. The
/// `display_name` falls back to `legal_name` (the operator can rename
/// later). `kind` is `Customer` — restored partners are the buyers on
/// the supplier's outgoing invoices. Optional address sub-fields ride
/// through verbatim.
fn customer_to_partner_inputs(customer: &CustomerCandidate) -> Result<PartnerInputs> {
    let legal_name = customer
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "customer candidate has no <customerName>; cannot derive partner \
                 display/legal name (required by §169 + validate_partner_inputs)"
            )
        })?
        .to_string();
    Ok(PartnerInputs {
        display_name: legal_name.clone(),
        legal_name,
        kind: PartnerKind::Customer,
        customer_vat_status: customer.vat_status,
        customer_type: crate::partners::CustomerType::Unset,
        tax_number: customer.tax_number.clone(),
        eu_vat_number: None,
        address_street: customer.address_street.clone(),
        address_postal_code: customer.address_postal_code.clone(),
        address_city: customer.address_city.clone(),
        address_country: customer.address_country.clone(),
        bank_account: None,
        contact_email: None,
        contact_phone: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProductUpsertOutcome {
    Inserted,
    SkippedSameKey,
    UpdatedPriceVaries,
}

fn upsert_product(
    conn: &Connection,
    tenant: &str,
    line: &LineCandidate,
    currency: Currency,
) -> Result<ProductUpsertOutcome> {
    match products::find_product_by_name_and_unit(conn, tenant, &line.description, &line.unit)? {
        Some(existing) => {
            if existing.unit_price_minor == line.unit_price_minor {
                return Ok(ProductUpsertOutcome::SkippedSameKey);
            }
            // Price drift — last-seen wins. WARN + UPDATE.
            tracing::warn!(
                product = existing.name.as_str(),
                stored_price_minor = existing.unit_price_minor,
                new_price_minor = line.unit_price_minor,
                "S196: product price drift detected across restored invoices; \
                 updating to new (last-seen) price (v3 polish: surface this \
                 as a `price_varies` flag on the product row)"
            );
            let new_inputs = ProductInputs {
                name: existing.name.clone(),
                unit: existing.unit.clone(),
                currency: existing.currency,
                unit_price_minor: line.unit_price_minor,
            };
            products::update_product(conn, tenant, &existing.id, &new_inputs)
                .context("update_product price after S196 drift")?
                .ok_or_else(|| {
                    anyhow!(
                        "product id `{}` vanished between find_product_by_name_and_unit \
                         and update_product — concurrent soft-delete?",
                        existing.id
                    )
                })?;
            Ok(ProductUpsertOutcome::UpdatedPriceVaries)
        }
        None => {
            let inputs = ProductInputs {
                name: line.description.trim().to_string(),
                unit: line.unit.clone(),
                currency,
                unit_price_minor: line.unit_price_minor,
            };
            products::create_product(conn, tenant, &inputs)
                .context("create_product from NAV-restored line candidate")?;
            Ok(ProductUpsertOutcome::Inserted)
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// XML / decimal helpers (local copies — keeping the extract module
// self-contained per CLAUDE.md rule 3 surgical-changes posture).
// ──────────────────────────────────────────────────────────────────────

/// HUF → 0 decimals (whole forints), EUR → 2 (cents). Mirrors
/// [`crate::restore_from_nav_outgoing::decimal_to_minor`]'s shape;
/// kept local to the extract module so the two callers do not need a
/// shared util crate (the brief's "minimum surface" posture).
fn decimal_to_minor(value: &str, currency: Currency) -> Result<i64> {
    let parsed: Decimal = value
        .parse()
        .map_err(|e| anyhow!("amount `{value}` is not a valid Decimal: {e}"))?;
    let scale: u32 = match currency {
        Currency::Huf => 0,
        Currency::Eur => 2,
    };
    let scaled = parsed * Decimal::from(10i64.pow(scale));
    scaled
        .round()
        .to_i64()
        .ok_or_else(|| anyhow!("amount `{value}` (scaled) exceeds i64 range"))
}

/// Namespace-blind local-name match. Mirrors
/// [`aberp_nav_transport::operations::find_first_text`]'s shape but is
/// re-implemented here so the extract module does not depend on a
/// crate-private helper.
fn find_first_text(xml: &[u8], target_local_name: &str) -> Result<Option<String>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut inside = false;
    let mut collected = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if local_name_matches(e.name().as_ref(), target_local_name) => {
                inside = true;
            }
            Ok(Event::End(e))
                if inside && local_name_matches(e.name().as_ref(), target_local_name) =>
            {
                return Ok(Some(collected));
            }
            Ok(Event::Text(t)) if inside => {
                let unescaped = t
                    .unescape()
                    .map_err(|e| anyhow!("XML text unescape failed: {e}"))?
                    .into_owned();
                collected.push_str(&unescaped);
            }
            Ok(Event::Eof) => return Ok(None),
            Err(e) => {
                return Err(anyhow!(
                    "XML parse failed at position {}: {e}",
                    reader.buffer_position()
                ));
            }
            _ => {}
        }
        buf.clear();
    }
}

/// Extract the byte slice of the first occurrence of an element (its
/// inner XML, the bytes between the opening and closing tag). Used
/// to scope a sub-search to one block (e.g., `<customerInfo>`'s
/// inner contents) so a stray element with the same local name
/// elsewhere cannot leak in. Returns `None` if the element is absent.
fn extract_element_block(xml: &[u8], target_local_name: &str) -> Result<Option<Vec<u8>>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut start_byte: Option<usize> = None;
    let mut depth: i32 = 0;
    loop {
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if start_byte.is_none() && local_name_matches(e.name().as_ref(), target_local_name)
                {
                    start_byte = Some(reader.buffer_position() as usize);
                    depth = 1;
                } else if start_byte.is_some()
                    && local_name_matches(e.name().as_ref(), target_local_name)
                {
                    depth += 1;
                }
            }
            Ok(Event::End(e))
                if start_byte.is_some()
                    && local_name_matches(e.name().as_ref(), target_local_name) =>
            {
                depth -= 1;
                if depth == 0 {
                    let start = start_byte.unwrap();
                    let end = pos_before;
                    let block = xml[start..end].to_vec();
                    return Ok(Some(block));
                }
            }
            Ok(Event::Eof) => return Ok(None),
            Err(e) => {
                return Err(anyhow!(
                    "XML parse failed at position {}: {e}",
                    reader.buffer_position()
                ));
            }
            _ => {}
        }
        buf.clear();
    }
}

/// Byte offset (in `xml`) one past the closing tag of the first
/// occurrence of `target_local_name`. Used by `parse_invoice_lines`
/// to advance its cursor between consecutive `<line>` siblings.
fn locate_element_end(xml: &[u8], target_local_name: &str) -> Result<Option<usize>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut started = false;
    let mut depth: i32 = 0;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if local_name_matches(e.name().as_ref(), target_local_name) => {
                if !started {
                    started = true;
                    depth = 1;
                } else {
                    depth += 1;
                }
            }
            Ok(Event::End(e))
                if started && local_name_matches(e.name().as_ref(), target_local_name) =>
            {
                depth -= 1;
                if depth == 0 {
                    return Ok(Some(reader.buffer_position() as usize));
                }
            }
            Ok(Event::Eof) => return Ok(None),
            Err(e) => {
                return Err(anyhow!(
                    "XML parse failed at position {}: {e}",
                    reader.buffer_position()
                ));
            }
            _ => {}
        }
        buf.clear();
    }
}

/// Match a quick-xml `QName` bytes blob against a target local name
/// regardless of namespace prefix. `b"common:foo"` matches `"foo"`,
/// and `b"foo"` (no prefix) also matches `"foo"`.
fn local_name_matches(qname: &[u8], target: &str) -> bool {
    let local = match qname.iter().position(|&b| b == b':') {
        Some(pos) => &qname[pos + 1..],
        None => qname,
    };
    local == target.as_bytes()
}

// ──────────────────────────────────────────────────────────────────────
// Path-scoped DB handle helper.
// ──────────────────────────────────────────────────────────────────────

/// Open a fresh DuckDB connection rooted at `db_path` and immediately
/// ensure both `partners` and `products` schemas. Used by the wizard's
/// per-invoice extraction step so the surface stays one function call
/// from `restore_from_nav_outgoing.rs` (mirrors the
/// `Connection::open` posture inside `process_digest`).
pub fn open_for_extract(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).with_context(|| {
        format!(
            "open tenant DuckDB at {} for S196 catalog extraction",
            db_path.display()
        )
    })?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .context("ADR-0098 R3 (finding C): disable implicit close-checkpoint on residual opener")?;
    partners::ensure_schema(&conn).context("ensure partners schema for S196 extraction")?;
    products::ensure_schema(&conn).context("ensure products schema for S196 extraction")?;
    Ok(conn)
}

// ──────────────────────────────────────────────────────────────────────
// Tests.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test tempdir mirroring [`crate::restore_from_nav_outgoing::tests::ScopedTempDir`].
    struct ScopedTempDir(std::path::PathBuf);

    impl ScopedTempDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir()
                .join(format!("aberp-s196-extract-{label}-{pid}-{nanos}-{seq}"));
            std::fs::create_dir_all(&path).expect("create scoped tempdir");
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScopedTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn domestic_invoice_inner_xml(
        customer_name: &str,
        tax_id: &str,
        vat_code: &str,
        county: &str,
        lines: &[(&str, &str, &str)],
    ) -> Vec<u8> {
        let mut lines_xml = String::new();
        for (i, (desc, unit, price)) in lines.iter().enumerate() {
            lines_xml.push_str(&format!(
                "<line><lineNumber>{}</lineNumber>\
                 <lineDescription>{}</lineDescription>\
                 <quantity>1</quantity>\
                 <unitOfMeasure>{}</unitOfMeasure>\
                 <unitPrice>{}</unitPrice></line>",
                i + 1,
                desc,
                unit,
                price,
            ));
        }
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<InvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data"
             xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <invoiceMain>
    <invoice>
      <invoiceHead>
        <customerInfo>
          <customerVatStatus>DOMESTIC</customerVatStatus>
          <customerVatData>
            <customerTaxNumber>
              <common:taxpayerId>{tax_id}</common:taxpayerId>
              <common:vatCode>{vat_code}</common:vatCode>
              <common:countyCode>{county}</common:countyCode>
            </customerTaxNumber>
          </customerVatData>
          <customerName>{customer_name}</customerName>
          <customerAddress>
            <common:simpleAddress>
              <common:countryCode>HU</common:countryCode>
              <common:postalCode>1023</common:postalCode>
              <common:city>Budapest</common:city>
              <common:additionalAddressDetail>Margit krt. 1.</common:additionalAddressDetail>
            </common:simpleAddress>
          </customerAddress>
        </customerInfo>
      </invoiceHead>
      <invoiceLines>{lines_xml}</invoiceLines>
    </invoice>
  </invoiceMain>
</InvoiceData>"#
        )
        .into_bytes()
    }

    fn private_person_invoice_inner_xml(
        customer_name: &str,
        country: &str,
        postal: &str,
        city: &str,
        street: &str,
    ) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<InvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data"
             xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <invoiceMain>
    <invoice>
      <invoiceHead>
        <customerInfo>
          <customerVatStatus>PRIVATE_PERSON</customerVatStatus>
          <customerName>{customer_name}</customerName>
          <customerAddress>
            <common:simpleAddress>
              <common:countryCode>{country}</common:countryCode>
              <common:postalCode>{postal}</common:postalCode>
              <common:city>{city}</common:city>
              <common:additionalAddressDetail>{street}</common:additionalAddressDetail>
            </common:simpleAddress>
          </customerAddress>
        </customerInfo>
      </invoiceHead>
      <invoiceLines>
        <line><lineNumber>1</lineNumber>
          <lineDescription>Konzultáció</lineDescription>
          <quantity>1</quantity>
          <unitOfMeasure>HOUR</unitOfMeasure>
          <unitPrice>25000</unitPrice></line>
      </invoiceLines>
    </invoice>
  </invoiceMain>
</InvoiceData>"#
        )
        .into_bytes()
    }

    // ── Parser pins ────────────────────────────────────────────────

    #[test]
    fn parse_customer_info_domestic_recomposes_dashed_tax_number() {
        let xml = domestic_invoice_inner_xml(
            "Teszt Kft.",
            "12345678",
            "2",
            "41",
            &[("Item", "PIECE", "1000")],
        );
        let candidate = parse_customer_info(&xml).expect("DOMESTIC parses");
        assert_eq!(candidate.vat_status, CustomerVatStatus::Domestic);
        assert_eq!(candidate.tax_number.as_deref(), Some("12345678-2-41"));
        assert_eq!(candidate.name.as_deref(), Some("Teszt Kft."));
        assert_eq!(candidate.address_country.as_deref(), Some("HU"));
        assert_eq!(candidate.address_postal_code.as_deref(), Some("1023"));
        assert_eq!(candidate.address_city.as_deref(), Some("Budapest"));
        assert_eq!(candidate.address_street.as_deref(), Some("Margit krt. 1."));
    }

    #[test]
    fn parse_customer_info_private_person_no_tax_number() {
        let xml = private_person_invoice_inner_xml(
            "Kovács Béla",
            "HU",
            "1041",
            "Budapest",
            "Árpád út 5.",
        );
        let candidate = parse_customer_info(&xml).expect("PrivatePerson parses");
        assert_eq!(candidate.vat_status, CustomerVatStatus::PrivatePerson);
        assert!(candidate.tax_number.is_none());
        assert_eq!(candidate.name.as_deref(), Some("Kovács Béla"));
        assert_eq!(candidate.address_street.as_deref(), Some("Árpád út 5."));
    }

    #[test]
    fn parse_invoice_lines_handles_nav_and_own_units() {
        let xml = r#"<InvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data">
  <invoiceMain><invoice><invoiceLines>
    <line><lineNumber>1</lineNumber>
      <lineDescription>Tanácsadás</lineDescription>
      <quantity>1</quantity>
      <unitOfMeasure>HOUR</unitOfMeasure>
      <unitPrice>25000</unitPrice></line>
    <line><lineNumber>2</lineNumber>
      <lineDescription>Gázolaj</lineDescription>
      <quantity>5</quantity>
      <unitOfMeasure>OWN</unitOfMeasure>
      <unitOfMeasureOwn>liter@15C</unitOfMeasureOwn>
      <unitPrice>650</unitPrice></line>
  </invoiceLines></invoice></invoiceMain></InvoiceData>"#
            .to_string();
        let lines = parse_invoice_lines(xml.as_bytes(), Currency::Huf).expect("parses");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].description, "Tanácsadás");
        assert_eq!(lines[0].unit, ProductUnit::Nav(NavUnitOfMeasure::Hour));
        assert_eq!(lines[0].unit_price_minor, 25_000);
        assert_eq!(lines[1].description, "Gázolaj");
        assert_eq!(lines[1].unit, ProductUnit::Own("liter@15C".to_string()));
        assert_eq!(lines[1].unit_price_minor, 650);
    }

    #[test]
    fn parse_invoice_lines_eur_scales_unit_price_to_cents() {
        let xml = r#"<InvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data">
  <invoiceMain><invoice><invoiceLines>
    <line><lineDescription>X</lineDescription>
      <unitOfMeasure>PIECE</unitOfMeasure>
      <unitPrice>12.34</unitPrice></line>
  </invoiceLines></invoice></invoiceMain></InvoiceData>"#;
        let lines = parse_invoice_lines(xml.as_bytes(), Currency::Eur).expect("parses");
        assert_eq!(lines[0].unit_price_minor, 1234);
    }

    #[test]
    fn parse_invoice_lines_loud_fails_on_own_without_companion() {
        let xml = r#"<InvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data">
  <invoiceMain><invoice><invoiceLines>
    <line><lineDescription>X</lineDescription>
      <unitOfMeasure>OWN</unitOfMeasure>
      <unitPrice>100</unitPrice></line>
  </invoiceLines></invoice></invoiceMain></InvoiceData>"#;
        let err =
            parse_invoice_lines(xml.as_bytes(), Currency::Huf).expect_err("OWN needs companion");
        assert!(format!("{err:#}").contains("unitOfMeasureOwn"));
    }

    #[test]
    fn extract_inner_invoice_data_xml_base64_round_trip() {
        // Build a queryInvoiceData response carrying a base64'd inner blob.
        let inner = b"<InvoiceData><x/></InvoiceData>";
        let b64 = base64::engine::general_purpose::STANDARD.encode(inner);
        let envelope = format!(
            r#"<QueryInvoiceDataResponse>
              <invoiceDataResult>
                <invoiceData>{b64}</invoiceData>
              </invoiceDataResult>
            </QueryInvoiceDataResponse>"#
        );
        let decoded = extract_inner_invoice_data_xml(envelope.as_bytes())
            .expect("decodes")
            .expect("Some(bytes) on present + valid base64");
        assert_eq!(decoded, inner);
    }

    /// PR-215 / S217 — RENAMED + FLIPPED. The pre-PR-215
    /// `_loud_fails_on_missing` pin was contract-wrong: per the NAV
    /// OSA 3.0 XSD `invoiceDataResult` is `minOccurs=0`, so funcCode=OK
    /// without `<invoiceData>` is a legitimate NAV response (observed
    /// on 13/13 INBOUND rows on 2026-06-01 prod). The parser MUST
    /// distinguish "absent" (Ok(None)) from "present but malformed"
    /// (Err) so the INBOUND caller can treat the former as expected
    /// and the latter as a contract violation. See the parser's
    /// doc-comment for the full return-shape contract.
    #[test]
    fn extract_inner_invoice_data_xml_returns_none_on_absent_per_pr215() {
        let envelope = b"<QueryInvoiceDataResponse></QueryInvoiceDataResponse>";
        let got = extract_inner_invoice_data_xml(envelope).expect("absent must not error");
        assert!(
            got.is_none(),
            "absent <invoiceData> must yield Ok(None); got {got:?}"
        );
    }

    /// PR-215 / S217 — same as above with the `<invoiceDataResult/>`
    /// self-closing wrapper present. Both `<invoiceDataResult>` absent
    /// AND empty must yield `Ok(None)` — the parser keys on
    /// `<invoiceData>` (the only element that carries the payload),
    /// not on the wrapper.
    #[test]
    fn extract_inner_invoice_data_xml_returns_none_on_empty_wrapper_per_pr215() {
        let envelope = b"<QueryInvoiceDataResponse>\
            <result><funcCode>OK</funcCode></result>\
            <invoiceDataResult/>\
        </QueryInvoiceDataResponse>";
        let got = extract_inner_invoice_data_xml(envelope).expect("empty wrapper must not error");
        assert!(
            got.is_none(),
            "empty <invoiceDataResult/> must yield Ok(None); got {got:?}"
        );
    }

    /// PR-215 / S217 — preserved loud-fail surface: `<invoiceData>`
    /// PRESENT with non-base64 content remains a contract violation
    /// per CLAUDE.md rule 12. The INBOUND caller will write the raw
    /// response to `.failed/` for triage.
    #[test]
    fn extract_inner_invoice_data_xml_loud_fails_on_invalid_base64_per_pr215() {
        let envelope = b"<QueryInvoiceDataResponse>\
            <invoiceDataResult><invoiceData>!!!not-base64!!!</invoiceData></invoiceDataResult>\
        </QueryInvoiceDataResponse>";
        let err =
            extract_inner_invoice_data_xml(envelope).expect_err("invalid base64 must loud-fail");
        assert!(
            format!("{err:#}").contains("base64-decode <invoiceData>"),
            "loud-fail must name the base64 decode failure; got: {err:#}"
        );
    }

    /// PR-215 / S217 — preserved loud-fail surface: `<invoiceData>`
    /// PRESENT with an empty / whitespace-only text node is a contract
    /// violation. Distinct from absence (which is Ok(None)).
    #[test]
    fn extract_inner_invoice_data_xml_loud_fails_on_empty_text_per_pr215() {
        let envelope = b"<QueryInvoiceDataResponse>\
            <invoiceDataResult><invoiceData>   </invoiceData></invoiceDataResult>\
        </QueryInvoiceDataResponse>";
        let err =
            extract_inner_invoice_data_xml(envelope).expect_err("present-but-empty must loud-fail");
        assert!(
            format!("{err:#}").contains("empty <invoiceData>"),
            "loud-fail must name the empty-blob case; got: {err:#}"
        );
    }

    // ── apply_candidates — dedup pins ─────────────────────────────

    fn open_db(label: &str) -> (ScopedTempDir, Connection) {
        let tmp = ScopedTempDir::new(label);
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = open_for_extract(&db_path).expect("open + ensure schemas");
        (tmp, conn)
    }

    fn domestic_candidate(name: &str, tax: &str) -> CustomerCandidate {
        CustomerCandidate {
            vat_status: CustomerVatStatus::Domestic,
            tax_number: Some(tax.to_string()),
            name: Some(name.to_string()),
            address_country: Some("HU".to_string()),
            address_postal_code: Some("1023".to_string()),
            address_city: Some("Budapest".to_string()),
            address_street: Some("Margit krt. 1.".to_string()),
        }
    }

    fn private_person_candidate(name: &str, street: &str) -> CustomerCandidate {
        CustomerCandidate {
            vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            name: Some(name.to_string()),
            address_country: Some("HU".to_string()),
            address_postal_code: Some("1041".to_string()),
            address_city: Some("Budapest".to_string()),
            address_street: Some(street.to_string()),
        }
    }

    fn nav_line(desc: &str, unit: NavUnitOfMeasure, price_minor: i64) -> LineCandidate {
        LineCandidate {
            description: desc.to_string(),
            unit: ProductUnit::Nav(unit),
            unit_price_minor: price_minor,
        }
    }

    #[test]
    fn apply_inserts_new_domestic_partner_once_then_dedups_by_tax_number() {
        let (_tmp, conn) = open_db("dom-dedup");
        let cust = domestic_candidate("Teszt Kft.", "12345678-2-41");
        let line = nav_line("Item", NavUnitOfMeasure::Piece, 1000);

        let d1 = apply_candidates(
            &conn,
            "t1",
            "INV/1",
            &cust,
            std::slice::from_ref(&line),
            Currency::Huf,
        );
        assert_eq!(d1.partners_restored, 1);
        assert_eq!(d1.partners_skipped_duplicate, 0);

        // Second invoice from the SAME customer → partner deduped.
        let d2 = apply_candidates(
            &conn,
            "t1",
            "INV/2",
            &cust,
            std::slice::from_ref(&line),
            Currency::Huf,
        );
        assert_eq!(d2.partners_restored, 0);
        assert_eq!(d2.partners_skipped_duplicate, 1);
    }

    #[test]
    fn apply_private_person_dedup_keys_on_name_and_address() {
        let (_tmp, conn) = open_db("pp-dedup");
        let cust_a = private_person_candidate("Kovács Béla", "Árpád út 5.");
        let cust_b = private_person_candidate("Kovács Béla", "Margit krt. 7."); // different street
        let line = nav_line("X", NavUnitOfMeasure::Piece, 100);

        let d1 = apply_candidates(
            &conn,
            "t1",
            "I/1",
            &cust_a,
            std::slice::from_ref(&line),
            Currency::Huf,
        );
        let d2 = apply_candidates(
            &conn,
            "t1",
            "I/2",
            &cust_a,
            std::slice::from_ref(&line),
            Currency::Huf,
        );
        let d3 = apply_candidates(
            &conn,
            "t1",
            "I/3",
            &cust_b,
            std::slice::from_ref(&line),
            Currency::Huf,
        );
        assert_eq!(d1.partners_restored, 1);
        assert_eq!(d2.partners_skipped_duplicate, 1, "same name+address dedup");
        assert_eq!(
            d3.partners_restored, 1,
            "different street → distinct partner"
        );
    }

    #[test]
    fn apply_private_person_dedup_is_case_insensitive_on_name_and_address() {
        let (_tmp, conn) = open_db("pp-case");
        let cust = private_person_candidate("Kovács Béla", "Árpád út 5.");
        let cust_upper = CustomerCandidate {
            name: Some("KOVÁCS BÉLA".to_string()),
            address_street: Some("ÁRPÁD ÚT 5.".to_string()),
            ..cust.clone()
        };
        let line = nav_line("X", NavUnitOfMeasure::Piece, 100);
        let d1 = apply_candidates(
            &conn,
            "t1",
            "I/1",
            &cust,
            std::slice::from_ref(&line),
            Currency::Huf,
        );
        let d2 = apply_candidates(
            &conn,
            "t1",
            "I/2",
            &cust_upper,
            std::slice::from_ref(&line),
            Currency::Huf,
        );
        assert_eq!(d1.partners_restored, 1);
        assert_eq!(
            d2.partners_skipped_duplicate, 1,
            "case-insensitive dedup must collapse name+address"
        );
    }

    #[test]
    fn apply_private_person_without_name_loud_fails_partner_only() {
        let (_tmp, conn) = open_db("pp-no-name");
        let cust = CustomerCandidate {
            vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            name: None,
            address_country: Some("HU".to_string()),
            address_postal_code: Some("1041".to_string()),
            address_city: Some("Budapest".to_string()),
            address_street: Some("Árpád út 5.".to_string()),
        };
        let line = nav_line("X", NavUnitOfMeasure::Piece, 100);
        let d = apply_candidates(
            &conn,
            "t1",
            "I/1",
            &cust,
            std::slice::from_ref(&line),
            Currency::Huf,
        );
        assert_eq!(d.partners_errored, 1, "missing name → partner errored");
        // The product extraction still ran successfully.
        assert_eq!(d.products_restored, 1, "product extraction is independent");
    }

    #[test]
    fn apply_products_dedup_by_name_and_unit_then_update_on_price_drift() {
        let (_tmp, conn) = open_db("prod-drift");
        let cust = domestic_candidate("Teszt Kft.", "12345678-2-41");
        let line_a = nav_line("Tanácsadás", NavUnitOfMeasure::Hour, 25_000);
        let line_b = nav_line("Tanácsadás", NavUnitOfMeasure::Hour, 30_000);

        let d1 = apply_candidates(
            &conn,
            "t1",
            "I/1",
            &cust,
            std::slice::from_ref(&line_a),
            Currency::Huf,
        );
        assert_eq!(d1.products_restored, 1);
        // Second invoice — same product, different price → drift counter,
        // not a fresh product row.
        let d2 = apply_candidates(
            &conn,
            "t1",
            "I/2",
            &cust,
            std::slice::from_ref(&line_b),
            Currency::Huf,
        );
        assert_eq!(d2.products_restored, 0, "duplicate by (name, unit)");
        assert_eq!(d2.products_skipped_duplicate, 1);
        assert_eq!(d2.products_price_varies, 1, "price drift surfaces loud");

        // Read-back: stored price is the LATEST (last-seen wins).
        let listed = products::list_products(&conn, "t1", Some("Tanácsadás")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].unit_price_minor, 30_000, "last-seen wins");
    }

    #[test]
    fn apply_products_distinct_units_stay_distinct_rows() {
        let (_tmp, conn) = open_db("prod-units");
        let cust = domestic_candidate("Teszt Kft.", "12345678-2-41");
        let line_hour = nav_line("Tanácsadás", NavUnitOfMeasure::Hour, 25_000);
        let line_day = nav_line("Tanácsadás", NavUnitOfMeasure::Day, 200_000);

        apply_candidates(
            &conn,
            "t1",
            "I/1",
            &cust,
            std::slice::from_ref(&line_hour),
            Currency::Huf,
        );
        apply_candidates(
            &conn,
            "t1",
            "I/2",
            &cust,
            std::slice::from_ref(&line_day),
            Currency::Huf,
        );
        let listed = products::list_products(&conn, "t1", Some("Tanácsadás")).unwrap();
        assert_eq!(
            listed.len(),
            2,
            "different unit → different product (name, unit dedup key)"
        );
    }

    #[test]
    fn apply_handles_multiple_lines_in_one_invoice() {
        let (_tmp, conn) = open_db("multi-line");
        let cust = domestic_candidate("Teszt Kft.", "12345678-2-41");
        let lines = vec![
            nav_line("A", NavUnitOfMeasure::Piece, 100),
            nav_line("A", NavUnitOfMeasure::Piece, 100), // same key → dedup
            nav_line("B", NavUnitOfMeasure::Piece, 200),
        ];
        let d = apply_candidates(&conn, "t1", "I/1", &cust, &lines, Currency::Huf);
        assert_eq!(d.products_restored, 2);
        assert_eq!(d.products_skipped_duplicate, 1);
    }

    #[test]
    fn apply_other_vat_status_loud_fails_partner_extraction() {
        let (_tmp, conn) = open_db("other-status");
        let cust = CustomerCandidate {
            vat_status: CustomerVatStatus::Other,
            tax_number: None,
            name: Some("Foreign Buyer".to_string()),
            address_country: Some("DE".to_string()),
            address_postal_code: Some("10115".to_string()),
            address_city: Some("Berlin".to_string()),
            address_street: Some("Unter den Linden 1".to_string()),
        };
        let d = apply_candidates(&conn, "t1", "I/1", &cust, &[], Currency::Huf);
        assert_eq!(
            d.partners_errored, 1,
            "OTHER vat_status is named-deferred per ADR-0048 §7"
        );
    }

    // ── ExtractionDelta accumulator pin ────────────────────────────

    #[test]
    fn extraction_delta_accumulates_each_field() {
        let mut acc = ExtractionDelta::default();
        acc.add(ExtractionDelta {
            partners_restored: 1,
            partners_skipped_duplicate: 2,
            partners_errored: 3,
            products_restored: 4,
            products_skipped_duplicate: 5,
            products_errored: 6,
            products_price_varies: 7,
            invoice_extraction_errored: 8,
        });
        acc.add(ExtractionDelta {
            partners_restored: 10,
            ..Default::default()
        });
        assert_eq!(acc.partners_restored, 11);
        assert_eq!(acc.partners_skipped_duplicate, 2);
        assert_eq!(acc.partners_errored, 3);
        assert_eq!(acc.products_restored, 4);
        assert_eq!(acc.products_skipped_duplicate, 5);
        assert_eq!(acc.products_errored, 6);
        assert_eq!(acc.products_price_varies, 7);
        assert_eq!(acc.invoice_extraction_errored, 8);
    }

    // ── PR-216 / S218 — apply_candidates writes back buyer snapshot ─

    /// Seed a `restored_invoice` row, then run `apply_candidates` for
    /// a matching `source_nav_invoice_number`. The row's buyer columns
    /// MUST come out populated from the parsed `CustomerCandidate`.
    /// Pins the cross-module integration: extract.rs writes back into
    /// the restore_from_nav_outgoing module's table via the public
    /// `update_buyer_fields` helper. Mirrors the prod fresh-restore
    /// path exactly.
    #[test]
    fn apply_candidates_writes_buyer_snapshot_to_restored_invoice() {
        use crate::restore_from_nav_outgoing as restore_outgoing;
        let (tmp, _conn) = open_db("s218-write-back");
        let db_path = tmp.path().join("aberp.duckdb");

        // Seed a restored_invoice row directly (mirrors the wizard's
        // process_digest INSERT, but bypasses the audit-ledger
        // dependency the wizard's full entry-point carries).
        let conn = Connection::open(&db_path).expect("open");
        restore_outgoing::ensure_schema(&conn).expect("schema");
        conn.execute(
            "INSERT INTO restored_invoice (
                id, tenant_id, source_nav_invoice_number, source_nav_transaction_id,
                issue_date, total_net_minor, total_vat_minor, total_gross_minor,
                currency, restore_year, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            duckdb::params![
                "rinv_TEST",
                "t1",
                "BIL-2026-S218",
                Option::<&str>::None,
                "2026-04-15",
                100_000_i64,
                27_000_i64,
                127_000_i64,
                "HUF",
                2026_i32,
                "2026-04-15T00:00:00Z",
            ],
        )
        .expect("seed row");

        // Run apply_candidates. The customer block names a DOMESTIC
        // buyer; the buyer snapshot UPDATE happens before the
        // partner-master upsert per the docstring contract.
        let cust = domestic_candidate("Teszt Kft.", "12345678-2-41");
        let line = nav_line("Item", NavUnitOfMeasure::Piece, 1000);
        let _delta = apply_candidates(
            &conn,
            "t1",
            "BIL-2026-S218",
            &cust,
            std::slice::from_ref(&line),
            Currency::Huf,
        );

        // Read the row back via list_restored — the buyer snapshot
        // must be populated.
        let list = restore_outgoing::list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].customer_name.as_deref(), Some("Teszt Kft."));
        assert_eq!(
            list[0].customer_tax_number.as_deref(),
            Some("12345678-2-41")
        );
        assert_eq!(list[0].customer_vat_status.as_deref(), Some("Domestic"));
    }

    /// PRIVATE_PERSON write-back path: tax_number stays None on the
    /// row (NAV's XSD forbids `<customerVatData>` for that vat
    /// status), but the name + vat_status DO populate.
    #[test]
    fn apply_candidates_writes_private_person_without_tax_number() {
        use crate::restore_from_nav_outgoing as restore_outgoing;
        let (tmp, _conn) = open_db("s218-pp");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        restore_outgoing::ensure_schema(&conn).expect("schema");
        conn.execute(
            "INSERT INTO restored_invoice (
                id, tenant_id, source_nav_invoice_number, source_nav_transaction_id,
                issue_date, total_net_minor, total_vat_minor, total_gross_minor,
                currency, restore_year, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            duckdb::params![
                "rinv_PP",
                "t1",
                "EU/2026/PP",
                Option::<&str>::None,
                "2026-05-01",
                50_000_i64,
                13_500_i64,
                63_500_i64,
                "EUR",
                2026_i32,
                "2026-05-01T00:00:00Z",
            ],
        )
        .expect("seed");

        let cust = private_person_candidate("Kovács Béla", "Árpád út 5.");
        let _delta = apply_candidates(&conn, "t1", "EU/2026/PP", &cust, &[], Currency::Eur);

        let list = restore_outgoing::list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].customer_name.as_deref(), Some("Kovács Béla"));
        assert_eq!(
            list[0].customer_tax_number, None,
            "PRIVATE_PERSON MUST NOT carry a tax number on the snapshot",
        );
        assert_eq!(
            list[0].customer_vat_status.as_deref(),
            Some("PrivatePerson")
        );
    }

    /// End-to-end golden: feed a verbatim NAV inner-XML payload (the
    /// shape `queryInvoiceData OUTBOUND` returns after base64-decode)
    /// through `parse_customer_info` + `update_buyer_fields`. Pins the
    /// HUF arm — mirrors Ervin's prod-imported invoices on 2026-06-01
    /// that show a missing partner column. A regression in the parser
    /// or the UPDATE shape surfaces here loud.
    #[test]
    fn end_to_end_huf_invoice_xml_populates_partner_column() {
        use crate::restore_from_nav_outgoing as restore_outgoing;
        let (tmp, _conn) = open_db("s218-e2e-huf");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        restore_outgoing::ensure_schema(&conn).expect("schema");
        conn.execute(
            "INSERT INTO restored_invoice (
                id, tenant_id, source_nav_invoice_number, source_nav_transaction_id,
                issue_date, total_net_minor, total_vat_minor, total_gross_minor,
                currency, restore_year, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            duckdb::params![
                "rinv_HUF",
                "t1",
                "BIL-2026-0042",
                Option::<&str>::None,
                "2026-04-15",
                100_000_i64,
                27_000_i64,
                127_000_i64,
                "HUF",
                2026_i32,
                "2026-04-15T00:00:00Z",
            ],
        )
        .expect("seed HUF row");

        // Inner InvoiceData XML — what queryInvoiceData OUTBOUND
        // returns after base64-decode for a DOMESTIC buyer.
        let inner = domestic_invoice_inner_xml(
            "Áben Consulting Kft.",
            "24904362",
            "2",
            "41",
            &[("Konzultáció", "HOUR", "25000")],
        );
        let customer = parse_customer_info(&inner).expect("parse");
        let lines = parse_invoice_lines(&inner, Currency::Huf).expect("parse lines");
        apply_candidates(
            &conn,
            "t1",
            "BIL-2026-0042",
            &customer,
            &lines,
            Currency::Huf,
        );

        let list = restore_outgoing::list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].customer_name.as_deref(),
            Some("Áben Consulting Kft."),
            "HUF golden: <customerName> MUST surface on the wire shape's customer_name"
        );
        assert_eq!(
            list[0].customer_tax_number.as_deref(),
            Some("24904362-2-41"),
            "HUF golden: dashed tax number MUST round-trip"
        );
    }

    /// End-to-end golden: EUR arm. NAV emits the same `<customerInfo>`
    /// shape regardless of currency, but the EUR-leg pin proves that
    /// the apply path does NOT silently key on HUF anywhere in the
    /// buyer snapshot write-back. The 14 prod rows include both HUF
    /// and EUR; both must populate.
    #[test]
    fn end_to_end_eur_invoice_xml_populates_partner_column() {
        use crate::restore_from_nav_outgoing as restore_outgoing;
        let (tmp, _conn) = open_db("s218-e2e-eur");
        let db_path = tmp.path().join("aberp.duckdb");
        let conn = Connection::open(&db_path).expect("open");
        restore_outgoing::ensure_schema(&conn).expect("schema");
        conn.execute(
            "INSERT INTO restored_invoice (
                id, tenant_id, source_nav_invoice_number, source_nav_transaction_id,
                issue_date, total_net_minor, total_vat_minor, total_gross_minor,
                currency, restore_year, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            duckdb::params![
                "rinv_EUR",
                "t1",
                "EU/2026/001",
                Option::<&str>::None,
                "2026-05-01",
                50_000_i64,
                13_500_i64,
                63_500_i64,
                "EUR",
                2026_i32,
                "2026-05-01T00:00:00Z",
            ],
        )
        .expect("seed EUR row");

        let inner = domestic_invoice_inner_xml(
            "EU Buyer GmbH",
            "98765432",
            "1",
            "23",
            &[("Service", "PIECE", "100.00")],
        );
        let customer = parse_customer_info(&inner).expect("parse");
        let lines = parse_invoice_lines(&inner, Currency::Eur).expect("parse lines");
        apply_candidates(&conn, "t1", "EU/2026/001", &customer, &lines, Currency::Eur);

        let list = restore_outgoing::list_restored(&db_path, "t1").expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].customer_name.as_deref(), Some("EU Buyer GmbH"));
        assert_eq!(
            list[0].customer_tax_number.as_deref(),
            Some("98765432-1-23")
        );
        assert_eq!(list[0].customer_vat_status.as_deref(), Some("Domestic"));
    }
}
