//! Orchestration for the `aberp print-invoice` subcommand.
//!
//! PR-44ε.1 / A152 — the binary-side glue between the audit-ledger
//! `InvoiceDraftCreated` payload, the NAV XML on disk, the per-tenant
//! `seller.toml`, and the [`aberp_invoice_pdf`] renderer.
//!
//! Pipeline:
//!
//! 1. Open the audit ledger and walk for the most-recent
//!    `InvoiceDraftCreated` payload whose `invoice_id == --id`.
//!    Extract `nav_xml_path`, `currency`, and (for non-HUF)
//!    `exchange_rate` + `exchange_rate_date` + `huf_equivalent_total`.
//!    Loud-fail per CLAUDE.md rule 12 on every recoverable surface
//!    (`--id` unknown, `nav_xml_path == None`, etc.).
//! 2. Read the verbatim NAV `<InvoiceData>` bytes from disk
//!    (ADR-0031 §2 + PR-18 on-disk posture; per A155 the printed-
//!    invoice render is the THIRD on-disk consumer after wire-submit
//!    + retry-drain).
//! 3. Parse the NAV XML for the §1.a printed-invoice field set —
//!    parties, dates, payment-method, lines, totals. The parser
//!    consumes ONLY the elements §1.a names; everything else the
//!    NAV body carries is ignored (CLAUDE.md rule 2). The XML
//!    is the regulatory record per ADR-0031 §2 — we trust it.
//! 4. Read `~/.aberp/<tenant>/seller.toml` (or `--seller-toml
//!    <PATH>` override) for seller bank info (the NAV body carries
//!    the seller tax number + address but NOT bank account / IBAN /
//!    SWIFT — those land on the printed invoice only).
//! 5. Build the [`aberp_invoice_pdf::InvoiceModel`] and call
//!    [`aberp_invoice_pdf::render_invoice`].
//! 6. Write the PDF bytes to `--out`.
//!
//! # On-disk posture (A155 — new at THIS PR)
//!
//! The renderer reads NAV XML byte-verbatim from disk — same posture
//! as `retry_submission.rs` + `drain_pending_retries.rs` per
//! PR-44δ.1 / A151. NO re-render. NO MNB re-fetch. NO billing-row
//! consultation. The audit-ledger `InvoiceDraftCreated` payload
//! supplies the rate metadata; the NAV XML supplies the parties + line
//! content + amounts. If the audit ledger says HUF, the renderer's
//! HUF branch fires; if EUR, the rate metadata stamped at issuance
//! time (NOT a fresh MNB rate) drives every HUF-equivalent figure on
//! the printed invoice.
//!
//! This means the printed invoice is byte-deterministic given a
//! committed audit chain — the chain-rate-drift failure mode C6
//! prohibits is structurally absent here too.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::{
    self as billing, BankAccountSnapshot, Currency, NavUnitOfMeasure, RateMetadata,
};
use aberp_invoice_pdf::{render_invoice, InvoiceModel, LineItem as PdfLine, PartyInfo, TenantLogo};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use quick_xml::events::Event;
use quick_xml::Reader;
use rust_decimal::Decimal;
use serde::Deserialize;
use time::macros::format_description;
use time::Date;

use crate::audit_payloads::InvoiceDraftCreatedPayload;
use crate::binary_hash;
use crate::cli::PrintInvoiceArgs;
use crate::invoice_bank_snapshot::load_invoice_bank_snapshot_in_tx;
use crate::issue_invoice::{AddressJson, CustomerJson, InvoiceInputJson};
use crate::nav_xml::CustomerVatStatus;
use crate::serve::sibling_input_json_path;

// ──────────────────────────────────────────────────────────────────────
// Entry points
// ──────────────────────────────────────────────────────────────────────

/// A successfully rendered printed-invoice PDF.
///
/// Returned by [`render_to_bytes`] for callers that need to do something
/// other than write the bytes to a single on-disk path — at PR-44ε.UI
/// the SPA `/api/invoices/:id/pdf` route streams `pdf_bytes` to the
/// browser as `application/pdf` and the `invoice_number` is used
/// downstream for the `Content-Disposition` filename.
///
/// CLAUDE.md rule 13: the struct carries the two and only two fields
/// every caller needs (`invoice_number` for naming, `pdf_bytes` for the
/// body); a future `pages_rendered` / `currency` / `total` field would
/// add weight for a hypothetical caller and so stays absent until a
/// trigger surfaces.
pub struct RenderedInvoice {
    pub invoice_number: String,
    pub pdf_bytes: Vec<u8>,
}

/// CLI entry point — invoked from `main.rs`'s `Command::PrintInvoice`
/// arm. Thin wrapper over [`render_to_bytes`] that writes the rendered
/// bytes to the operator-supplied `--out` path and prints a one-line
/// summary.
pub fn run(args: &PrintInvoiceArgs) -> Result<()> {
    let _span = tracing::info_span!("print_invoice").entered();

    let rendered = render_to_bytes(
        &args.id,
        &args.db,
        &args.tenant,
        args.seller_toml.as_deref(),
    )?;

    fs::write(&args.out, &rendered.pdf_bytes)
        .with_context(|| format!("write printed-invoice PDF to {}", args.out.display()))?;

    tracing::info!(
        invoice_id = %args.id,
        out = %args.out.display(),
        bytes = rendered.pdf_bytes.len(),
        "printed-invoice PDF written"
    );
    println!(
        "printed invoice {} -> {} ({} bytes)",
        rendered.invoice_number,
        args.out.display(),
        rendered.pdf_bytes.len(),
    );
    Ok(())
}

/// Library-callable surface — produces the printed-invoice PDF bytes
/// without touching the file system at the output side. Consumed by
/// both [`run`] (which writes to `--out`) and the PR-44ε.UI
/// `GET /api/invoices/:id/pdf` route (which streams the bytes to the
/// browser).
///
/// The on-disk reads — audit ledger, NAV body byte-verbatim, seller
/// TOML — are unchanged from the pre-PR-44ε.UI shape per A155
/// (printed-invoice render is byte-deterministic given a committed
/// audit chain + the on-disk NAV body). The split is a refactor for
/// caller polymorphism, NOT a posture change.
///
/// `invoice_id` shape per `find_invoice_draft`: the prefixed-ULID form
/// the audit-ledger `InvoiceDraftCreated` payload's `invoice_id` field
/// carries. Not validated here — the ledger walk returns a clean
/// not-found error if no entry matches.
///
/// `seller_toml`: `Some(path)` for an explicit override (CLI's
/// `--seller-toml` or the test fixture); `None` to fall back to
/// `~/.aberp/<tenant>/seller.toml` per `resolve_seller_toml_path`.
pub fn render_to_bytes(
    invoice_id: &str,
    db: &Path,
    tenant: &str,
    seller_toml: Option<&Path>,
) -> Result<RenderedInvoice> {
    let tenant_id = TenantId::new(tenant.to_string())
        .ok_or_else(|| anyhow!("--tenant value '{}' is empty or has a null byte", tenant))?;

    // 1. Locate the InvoiceDraftCreated payload from the audit ledger.
    //    The binary hash is computed but only used to satisfy the
    //    LedgerMeta surface; the print-invoice path does NOT write to
    //    the ledger so the hash is not load-bearing here.
    let binary_hash_bytes: BinaryHash = binary_hash::compute().context("compute binary hash")?;
    let ledger = Ledger::open(db, tenant_id, binary_hash_bytes)
        .with_context(|| format!("open audit ledger at {}", db.display()))?;
    let draft = find_invoice_draft(&ledger, invoice_id)?;

    // 2. Read the verbatim NAV body bytes off disk.
    let xml_path: PathBuf = draft
        .nav_xml_path
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow!(
                "InvoiceDraftCreated for {} carries no nav_xml_path \
                 (pre-PR-18 entry); printed-invoice render requires the \
                 on-disk NAV body — recover the XML and pass --xml-path-override \
                 on a future PR-44ε.1.1 lift",
                invoice_id
            )
        })?;
    let xml_bytes =
        fs::read(&xml_path).with_context(|| format!("read NAV XML from {}", xml_path.display()))?;

    // 3. Parse NAV XML for the printed-invoice field set.
    let parsed = parse_nav_invoice_xml(&xml_bytes)
        .with_context(|| format!("parse NAV XML at {}", xml_path.display()))?;

    // 4. Resolve currency + rate metadata from the audit payload (NOT
    //    from the XML — the XML carries the wire body shape; the
    //    audit-ledger stamp is the regulatory record per ADR-0037 §3).
    let currency = parse_currency_from_payload(&draft, invoice_id)?;
    let rate_metadata = build_rate_metadata_from_payload(&draft, currency)?;

    // 5. Read the seller-info TOML (bank account / IBAN / SWIFT — fields
    //    that don't appear on the NAV body but are on the printed
    //    invoice per the reference template).
    let seller_toml_path = resolve_seller_toml_path(seller_toml, tenant)?;
    let seller_info = read_seller_toml(&seller_toml_path)
        .with_context(|| format!("read seller-info TOML at {}", seller_toml_path.display()))?;

    // PR-176 — load the optional tenant logo from `~/.aberp/<tenant>/logo.png`
    // (or the parent of `seller_toml` when an explicit override is in
    // play, mirroring the seller.toml resolution so test fixtures land
    // the logo next to the toml). Absent file → `None`, no error,
    // header falls back to text-only. Malformed PNG → loud error,
    // caller sees the failure. The convention is documented in
    // `README.md` + `docs/CUTOVER_RUNBOOK.md` per the brief.
    let tenant_logo = load_tenant_logo(&seller_toml_path)
        .with_context(|| format!("load tenant logo for {tenant} (PR-176)"))?;

    // S195 / PR-195 — load the optional brand primary colour from
    // `[seller.branding] primary_color` in `seller.toml`. Same posture
    // as the tenant logo: legal-document rendering must never be
    // blocked by a branding asset, so a malformed hex string downgrades
    // to a `tracing::warn!` + `None` (the pre-PR-195 ADR-0044 palette
    // is the silent default). An I/O error on the read still
    // propagates — that's a real configuration failure the operator
    // wants to know about, not a branding-asset noise case.
    let brand_primary_color = load_brand_primary_color(&seller_toml_path)?;

    // PR-86 / session-111 — read the per-invoice bank snapshot stamped
    // at issuance (PR-73 / ADR-0040 §addendum-C). For invoices issued
    // after PR-73, this is the regulatory record of WHICH bank
    // account the operator chose at issuance time — it survives
    // operator edits to `seller.toml` after the fact. PRE-PR-73
    // invoices return an empty snapshot; the renderer falls back to
    // the legacy flat-root fields read from `seller.toml` for those
    // rows so historical re-renders stay byte-stable.
    let bank_snapshot = load_invoice_bank_snapshot(db, invoice_id)
        .with_context(|| format!("load bank snapshot for invoice {invoice_id} (PR-86)"))?;

    // 6. Build the renderer model. The supplier's bank block prefers
    //    the per-invoice snapshot (PR-86) and falls back to the
    //    legacy `seller.toml` flat-root fields only when no snapshot
    //    was stamped (pre-PR-73 invoices).
    let supplier_bank = supplier_bank_fields(bank_snapshot.as_ref(), &seller_info);
    let supplier = PartyInfo {
        name: parsed.supplier_name.clone(),
        address_lines: parsed.supplier_address_lines.clone(),
        tax_number: parsed.supplier_tax_number.clone(),
        bank_account_number: supplier_bank.bank_account_number,
        iban: supplier_bank.iban,
        bank_name: supplier_bank.bank_name,
        swift_bic: supplier_bank.swift_bic,
    };
    // S168 — for PRIVATE_PERSON buyers the NAV wire suppresses
    // `<customerName>` + `<customerAddress>` per ADR-0048 amendment
    // 2026-05-29 (NAV business rule CUSTOMER_DATA_NOT_EXPECTED). The PDF
    // must still carry both fields per Áfa tv. §169 when the operator
    // entered them, so we re-source from the operator's audit-immutable
    // `<ULID>.input.json` side-store (PR-47α). For DOMESTIC / OTHER
    // invoices, and for CLI-issued invoices without an input.json
    // sibling, the NAV-XML values flow through unchanged.
    let operator_customer = load_operator_customer_input(&xml_path)?;
    let customer_pdf = derive_customer_pdf_fields(&parsed, operator_customer.as_ref());
    let customer = PartyInfo {
        name: customer_pdf.name,
        address_lines: customer_pdf.address_lines,
        tax_number: customer_pdf.tax_number,
        bank_account_number: None,
        iban: None,
        bank_name: None,
        swift_bic: None,
    };

    // PR-82 — buyer-facing notes ride OUTSIDE the NAV XML (never-leak
    // invariant; see `adr/0042-invoice-notes-never-in-nav-xml.md`). The
    // printed-PDF render is therefore the consumer that re-joins the
    // NAV XML (parties + amounts) with the DuckDB-stored notes
    // (`invoice.invoice_note` + `invoice_line.note`). The read happens
    // here, after the XML parse, in a fresh DuckDB tx — same posture as
    // the audit-ledger read above. Operator-twin record stays in the
    // audit-ledger payload too; the DuckDB read is the operational
    // index for fast lookup.
    let invoice_notes = load_invoice_notes(db, invoice_id)
        .with_context(|| format!("load buyer-facing notes for invoice {invoice_id} (PR-82)"))?;

    let lines: Vec<PdfLine> = parsed
        .lines
        .iter()
        .enumerate()
        .map(|(idx, l)| PdfLine {
            description: l.description.clone(),
            quantity: l.quantity,
            unit: unit_display_from_nav(&l.unit_of_measure, l.unit_of_measure_own.as_deref()),
            unit_price_minor: native_to_minor(&l.unit_price_native, currency),
            net_minor: native_to_minor(&l.net_native, currency),
            vat_rate_percent: l.vat_rate_percent,
            vat_minor: native_to_minor(&l.vat_native, currency),
            gross_minor: native_to_minor(&l.gross_native, currency),
            performance_period: None,
            // PR-82 — pair the per-line note off the DuckDB read by
            // ordinal. The NAV-XML line order is the regulatory wire
            // order (ordered by `ordinal` ascending at write time per
            // `allocate_in_tx`'s `invoice_line` INSERT loop), so
            // index-pairing here is sound. A drift on either side
            // would surface visibly: NAV line 1's gross next to the
            // wrong line's note.
            note: invoice_notes.line_notes.get(idx).cloned().unwrap_or(None),
        })
        .collect();

    let model = InvoiceModel {
        invoice_number: parsed.invoice_number.clone(),
        issue_date: parsed.issue_date,
        fulfillment_date: parsed.delivery_date.unwrap_or(parsed.issue_date),
        payment_due_date: parsed.payment_date.unwrap_or(parsed.issue_date),
        payment_method: payment_method_display(&parsed.payment_method),
        currency,
        rate_metadata,
        supplier,
        customer,
        lines,
        // PR-82 — invoice-level buyer note flows from the DuckDB read.
        note: invoice_notes.invoice_note,
        // PR-176 — optional tenant-logo for the printed header.
        tenant_logo,
        // S195 / PR-195 — optional brand colour applied to the title
        // under-rule, table-header rule, and totals banner. `None`
        // keeps the pre-PR-195 silver/gold palette byte-for-byte.
        brand_primary_color,
    };

    // 7. Render.
    let pdf_bytes = render_invoice(&model).context("render printed-invoice PDF")?;
    Ok(RenderedInvoice {
        invoice_number: parsed.invoice_number,
        pdf_bytes,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Audit-ledger lookup
// ──────────────────────────────────────────────────────────────────────

fn find_invoice_draft(ledger: &Ledger, invoice_id: &str) -> Result<InvoiceDraftCreatedPayload> {
    let entries = ledger
        .entries()
        .context("read audit-ledger entries to resolve InvoiceDraftCreated")?;
    // Walk newest → oldest; first matching draft wins.
    for entry in entries.iter().rev() {
        if entry.kind != EventKind::InvoiceDraftCreated {
            continue;
        }
        let payload: InvoiceDraftCreatedPayload = serde_json::from_slice(&entry.payload)
            .with_context(|| {
                format!(
                    "InvoiceDraftCreated payload (seq {:?}) failed typed decode",
                    entry.seq
                )
            })?;
        if payload.invoice_id == invoice_id {
            return Ok(payload);
        }
    }
    Err(anyhow!(
        "no InvoiceDraftCreated audit entry found for invoice id {} \
         — verify --id, --db, --tenant; if the invoice was issued under \
         a different tenant, switch --tenant",
        invoice_id
    ))
}

fn parse_currency_from_payload(
    payload: &InvoiceDraftCreatedPayload,
    invoice_id: &str,
) -> Result<Currency> {
    // Pre-PR-44γ entries serialise with `currency: None`; treat as HUF
    // per the same convention `audit_payloads.rs` documents on the
    // payload field comment.
    match payload.currency.as_deref() {
        None | Some("HUF") => Ok(Currency::Huf),
        Some("EUR") => Ok(Currency::Eur),
        Some(other) => Err(anyhow!(
            "InvoiceDraftCreated for {} has currency='{}'; PR-44ε.1 \
             renders the closed ADR-0037 §3 vocab only (HUF, EUR) — \
             a third currency variant is named-deferred per ADR-0037 §5",
            invoice_id,
            other
        )),
    }
}

fn build_rate_metadata_from_payload(
    payload: &InvoiceDraftCreatedPayload,
    currency: Currency,
) -> Result<Option<RateMetadata>> {
    if matches!(currency, Currency::Huf) {
        return Ok(None);
    }
    let rate_str = payload.exchange_rate.as_deref().ok_or_else(|| {
        anyhow!(
            "non-HUF InvoiceDraftCreated has no exchange_rate (DB row corrupt or \
             pre-PR-44γ entry on a non-HUF currency — both impossible under the \
             ADR-0037 §3 + §4 C1 invariants)"
        )
    })?;
    let source = payload
        .exchange_rate_source
        .as_deref()
        .ok_or_else(|| anyhow!("non-HUF InvoiceDraftCreated has no exchange_rate_source"))?
        .to_string();
    let date_str = payload
        .exchange_rate_date
        .as_deref()
        .ok_or_else(|| anyhow!("non-HUF InvoiceDraftCreated has no exchange_rate_date"))?;
    let huf_total_str = payload
        .huf_equivalent_total
        .as_deref()
        .ok_or_else(|| anyhow!("non-HUF InvoiceDraftCreated has no huf_equivalent_total"))?;

    let rate = Decimal::from_str(rate_str).map_err(|_| {
        anyhow!(
            "InvoiceDraftCreated.exchange_rate value `{}` is not a parseable decimal \
             — audit-ledger row corrupt",
            rate_str
        )
    })?;
    let date =
        Date::parse(date_str, &format_description!("[year]-[month]-[day]")).map_err(|e| {
            anyhow!(
                "InvoiceDraftCreated.exchange_rate_date `{}` does not parse as YYYY-MM-DD: {e}",
                date_str
            )
        })?;
    let huf_equivalent_total = i64::from_str(huf_total_str).map_err(|_| {
        anyhow!(
            "InvoiceDraftCreated.huf_equivalent_total `{}` is not a parseable i64 — \
             audit-ledger row corrupt",
            huf_total_str
        )
    })?;
    Ok(Some(RateMetadata {
        rate,
        source,
        date,
        huf_equivalent_total,
    }))
}

// ──────────────────────────────────────────────────────────────────────
// NAV XML parser (printed-invoice subset)
// ──────────────────────────────────────────────────────────────────────

/// Subset of NAV `<InvoiceData>` consumed by the printed-invoice
/// renderer. The wire body carries more than this; the parser
/// extracts ONLY the §1.a field set per CLAUDE.md rule 2.
#[derive(Debug, Clone)]
pub struct ParsedNavInvoice {
    pub invoice_number: String,
    pub issue_date: Date,
    pub delivery_date: Option<Date>,
    pub payment_date: Option<Date>,
    pub payment_method: String,
    pub supplier_tax_number: String,
    pub supplier_name: String,
    pub supplier_address_lines: Vec<String>,
    pub customer_tax_number: String,
    pub customer_name: String,
    /// Session-150 — buyer address lines parsed from `<customerAddress>`
    /// (the four `<common:simpleAddress>` children in document order:
    /// countryCode, postalCode, city, additionalAddressDetail). Empty
    /// for invoices whose NAV XML carries no `<customerAddress>` block
    /// — e.g. PrivatePerson invoices issued before session-150 made the
    /// buyer address §169-mandatory at preflight. Mirrors
    /// [`Self::supplier_address_lines`].
    pub customer_address_lines: Vec<String>,
    pub lines: Vec<ParsedNavLine>,
}

impl ParsedNavInvoice {
    fn empty() -> Self {
        Self {
            invoice_number: String::new(),
            // Sentinel — overwritten by the parser before return.
            // `time::Date` carries no `Default`, so we anchor on a
            // sentinel that surfaces obviously if a malformed XML
            // skipped the `<invoiceIssueDate>` write path.
            issue_date: Date::from_calendar_date(1970, time::Month::January, 1)
                .expect("1970-01-01 is a valid calendar date"),
            delivery_date: None,
            payment_date: None,
            payment_method: String::new(),
            supplier_tax_number: String::new(),
            supplier_name: String::new(),
            supplier_address_lines: Vec::new(),
            customer_tax_number: String::new(),
            customer_name: String::new(),
            customer_address_lines: Vec::new(),
            lines: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ParsedNavLine {
    pub description: String,
    /// S157 — decimal quantity parsed verbatim from the `<quantity>`
    /// element. Pre-S157 this was `u32` and the parser truncated `1.5` →
    /// `1` on the printed PDF; the renderer now shows the full value with
    /// the Hungarian comma.
    pub quantity: Decimal,
    /// PR-202 — NAV `<unitOfMeasure>` element body verbatim (one of the
    /// closed-vocab tokens `PIECE`/`KILOGRAM`/…/`DAY`, the literal `OWN`,
    /// or — defensively — empty for a malformed/pre-S159 body that wrote
    /// no element). [`unit_display_from_nav`] consumes this together with
    /// [`Self::unit_of_measure_own`] to produce the operator-facing
    /// Hungarian label the PDF column renders.
    pub unit_of_measure: String,
    /// PR-202 — NAV `<unitOfMeasureOwn>` free-text companion. NAV's
    /// LineType permits this ONLY when `<unitOfMeasure>` is the literal
    /// `OWN`; for every closed-vocab variant it is absent and this stays
    /// `None`.
    pub unit_of_measure_own: Option<String>,
    /// Native-currency unit price as written on the wire — `"1000"` for
    /// HUF (integer forints), `"12.34"` for EUR (two-decimal cents
    /// rendered as euros-and-cents). Converted to minor units by
    /// [`native_to_minor`].
    pub unit_price_native: String,
    pub net_native: String,
    pub vat_rate_percent: u16,
    pub vat_native: String,
    pub gross_native: String,
}

/// Streaming parser. Walks the NAV body element-by-element, tracking a
/// breadcrumb path of element names. On a `Text` event whose path
/// matches a printed-invoice field, the value is captured. The parser
/// is intentionally small: no XSD validation (PR-9-0 / ADR-0022
/// already runs at issuance time — the bytes on disk are guaranteed
/// shape-valid), no namespace introspection (one namespace per body).
pub fn parse_nav_invoice_xml(bytes: &[u8]) -> Result<ParsedNavInvoice> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);

    let mut path: Vec<String> = Vec::new();
    let mut out = ParsedNavInvoice::empty();
    let mut cur_line: Option<ParsedNavLine> = None;
    let mut address_buf: Vec<String> = Vec::new();
    let mut address_in: Option<&'static str> = None;
    let mut buf = Vec::new();
    let date_fmt = format_description!("[year]-[month]-[day]");

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => {
                return Err(anyhow!(
                    "XML parse error at position {}: {e}",
                    reader.buffer_position()
                ))
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(local_name(e.name().as_ref())).to_string();
                path.push(name.clone());
                match name.as_str() {
                    "line"
                        if matches_path(
                            &path,
                            &[
                                "InvoiceData",
                                "invoiceMain",
                                "invoice",
                                "invoiceLines",
                                "line",
                            ],
                        ) =>
                    {
                        cur_line = Some(ParsedNavLine::default());
                    }
                    "supplierAddress" => {
                        address_in = Some("supplier");
                        address_buf.clear();
                    }
                    // Session-150 — buyer address, same simpleAddress
                    // child shape as supplierAddress. Reuses the shared
                    // `address_buf` (the two blocks are siblings, never
                    // nested) cleared on entry.
                    "customerAddress" => {
                        address_in = Some("customer");
                        address_buf.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(local_name(e.name().as_ref())).to_string();
                if name == "line" && cur_line.is_some() {
                    out.lines.push(cur_line.take().unwrap());
                }
                if name == "supplierAddress" && address_in == Some("supplier") {
                    out.supplier_address_lines = std::mem::take(&mut address_buf);
                    address_in = None;
                }
                if name == "customerAddress" && address_in == Some("customer") {
                    out.customer_address_lines = std::mem::take(&mut address_buf);
                    address_in = None;
                }
                let _popped = path.pop();
            }
            Ok(Event::Text(t)) => {
                let value = t
                    .unescape()
                    .map_err(|e| anyhow!("XML text decode failed: {e}"))?
                    .into_owned();
                handle_text(
                    &path,
                    &value,
                    &mut out,
                    &mut cur_line,
                    address_in,
                    &mut address_buf,
                    date_fmt,
                )?;
            }
            Ok(_) => {}
        }
        buf.clear();
    }

    if out.invoice_number.is_empty() {
        return Err(anyhow!("NAV XML has no <invoiceNumber>"));
    }
    if out.lines.is_empty() {
        return Err(anyhow!("NAV XML has no <invoiceLines>/<line>"));
    }
    Ok(out)
}

fn local_name(qualified: &[u8]) -> &[u8] {
    if let Some(idx) = qualified.iter().position(|&b| b == b':') {
        &qualified[idx + 1..]
    } else {
        qualified
    }
}

fn matches_path(path: &[String], expected: &[&str]) -> bool {
    if path.len() != expected.len() {
        return false;
    }
    path.iter().zip(expected.iter()).all(|(a, b)| a == b)
}

fn ends_with(path: &[String], suffix: &[&str]) -> bool {
    if path.len() < suffix.len() {
        return false;
    }
    let tail = &path[path.len() - suffix.len()..];
    tail.iter().zip(suffix.iter()).all(|(a, b)| a == b)
}

#[allow(clippy::too_many_arguments)]
fn handle_text(
    path: &[String],
    value: &str,
    out: &mut ParsedNavInvoice,
    cur_line: &mut Option<ParsedNavLine>,
    address_in: Option<&'static str>,
    address_buf: &mut Vec<String>,
    date_fmt: &[time::format_description::FormatItem<'_>],
) -> Result<()> {
    let last = match path.last() {
        Some(n) => n.as_str(),
        None => return Ok(()),
    };

    // ── Top-level / invoice-head fields ──────────────────────────────
    if address_in.is_none() {
        match last {
            "invoiceNumber" if ends_with(path, &["InvoiceData", "invoiceNumber"]) => {
                out.invoice_number = value.to_string();
            }
            "invoiceIssueDate" if ends_with(path, &["InvoiceData", "invoiceIssueDate"]) => {
                out.issue_date = Date::parse(value, date_fmt)
                    .map_err(|e| anyhow!("<invoiceIssueDate> `{value}` parse: {e}"))?;
            }
            "invoiceDeliveryDate" => {
                out.delivery_date = Some(
                    Date::parse(value, date_fmt)
                        .map_err(|e| anyhow!("<invoiceDeliveryDate> `{value}` parse: {e}"))?,
                );
            }
            "paymentDate" => {
                out.payment_date = Some(
                    Date::parse(value, date_fmt)
                        .map_err(|e| anyhow!("<paymentDate> `{value}` parse: {e}"))?,
                );
            }
            "paymentMethod" => {
                out.payment_method = value.to_string();
            }
            // PR-50 / session-70 — `<supplierTaxNumber>` is structured
            // (`<taxpayerId>` + `<vatCode>` + `<countyCode>`) per NAV
            // v3.0. Assemble the canonical `xxxxxxxx-y-zz` form for the
            // printed-invoice header by appending each child as it
            // streams in (XML guarantees the ordered children per the
            // emitter's writer order). The flat-string fallback at
            // path `["supplierInfo", "supplierTaxNumber"]` is gone —
            // structured-only is the wire shape post-PR-50.
            "taxpayerId" if ends_with(path, &["supplierTaxNumber", "taxpayerId"]) => {
                out.supplier_tax_number = value.to_string();
            }
            "vatCode" if ends_with(path, &["supplierTaxNumber", "vatCode"]) => {
                out.supplier_tax_number.push('-');
                out.supplier_tax_number.push_str(value);
            }
            "countyCode" if ends_with(path, &["supplierTaxNumber", "countyCode"]) => {
                out.supplier_tax_number.push('-');
                out.supplier_tax_number.push_str(value);
            }
            "supplierName" => {
                out.supplier_name = value.to_string();
            }
            "customerName" => {
                out.customer_name = value.to_string();
            }
            // PR-50 / session-70 — customer tax number same structured
            // shape as supplier: <taxpayerId> + <vatCode> + <countyCode>
            // assembled into canonical xxxxxxxx-y-zz form.
            "taxpayerId" if ends_with(path, &["customerTaxNumber", "taxpayerId"]) => {
                out.customer_tax_number = value.to_string();
            }
            "vatCode" if ends_with(path, &["customerTaxNumber", "vatCode"]) => {
                out.customer_tax_number.push('-');
                out.customer_tax_number.push_str(value);
            }
            "countyCode" if ends_with(path, &["customerTaxNumber", "countyCode"]) => {
                out.customer_tax_number.push('-');
                out.customer_tax_number.push_str(value);
            }
            _ => {}
        }
    }

    // ── Supplier address: collect every text under <supplierAddress> ──
    if address_in.is_some() {
        match last {
            "countryCode" | "postalCode" | "city" | "additionalAddressDetail"
                if !value.is_empty() =>
            {
                address_buf.push(value.to_string());
            }
            _ => {}
        }
    }

    // ── Per-line fields ──────────────────────────────────────────────
    if let Some(line) = cur_line.as_mut() {
        match last {
            "lineDescription" => line.description = value.to_string(),
            "quantity" => {
                // S157 — parse the full decimal quantity (NAV writes
                // `<quantity>` dot-separated). The renderer's
                // `LineItem::quantity` is now `Decimal`, so no truncation.
                line.quantity = Decimal::from_str(value)
                    .map_err(|e| anyhow!("<quantity> `{value}` parse: {e}"))?;
            }
            // PR-202 — capture the unit-of-measure pair so the PDF renders
            // the operator's actual unit instead of the pre-PR-202
            // hardcoded "PIECE". NAV's LineType places `<unitOfMeasure>`
            // after `<quantity>` and (when the value is `OWN`) follows
            // it with `<unitOfMeasureOwn>`; the parser captures both
            // verbatim and `unit_display_from_nav` resolves the
            // operator-facing label.
            "unitOfMeasure" => line.unit_of_measure = value.to_string(),
            "unitOfMeasureOwn" => line.unit_of_measure_own = Some(value.to_string()),
            "unitPrice" => line.unit_price_native = value.to_string(),
            "lineNetAmount" => line.net_native = value.to_string(),
            "lineVatAmount" => line.vat_native = value.to_string(),
            "lineGrossAmountNormal" => line.gross_native = value.to_string(),
            "vatPercentage" if ends_with(path, &["lineVatRate", "vatPercentage"]) => {
                // NAV writes the VAT rate as `0.27` (27%). Convert to
                // a u16 percent by × 100 + round.
                let dec: Decimal = Decimal::from_str(value)
                    .map_err(|e| anyhow!("<vatPercentage> `{value}` parse: {e}"))?;
                let pct = dec
                    .checked_mul(Decimal::from(100))
                    .ok_or_else(|| anyhow!("<vatPercentage> × 100 overflow"))?;
                let pct_rounded = pct.round();
                line.vat_rate_percent = rust_decimal::prelude::ToPrimitive::to_u16(&pct_rounded)
                    .ok_or_else(|| anyhow!("<vatPercentage> → u16 overflow"))?;
            }
            _ => {}
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Native-amount → minor-units conversion
// ──────────────────────────────────────────────────────────────────────

/// Convert a NAV wire-body native-currency amount to renderer minor
/// units. For HUF the wire form is an integer forint count and the
/// renderer's minor unit is the forint — passthrough. For EUR the
/// wire form is a two-decimal euro amount (`"12.34"`); the renderer
/// expects cents (`1234`).
pub fn native_to_minor(native: &str, currency: Currency) -> i64 {
    match currency {
        Currency::Huf => i64::from_str(native).unwrap_or(0),
        Currency::Eur => {
            let neg = native.starts_with('-');
            let body = native.trim_start_matches('-');
            let (whole, frac) = body.split_once('.').unwrap_or((body, ""));
            let whole_n: i64 = whole.parse().unwrap_or(0);
            let frac_padded: String = if frac.len() >= 2 {
                frac.chars().take(2).collect()
            } else {
                format!("{:0<2}", frac)
            };
            let frac_n: i64 = frac_padded.parse().unwrap_or(0);
            let unsigned = whole_n.saturating_mul(100).saturating_add(frac_n);
            if neg {
                -unsigned
            } else {
                unsigned
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// PR-82 — buyer-facing notes read
// ──────────────────────────────────────────────────────────────────────

/// PR-82 — buyer-facing notes for one invoice, fetched off DuckDB at
/// print time. The NAV XML on disk does NOT carry notes (never-leak
/// invariant; see `adr/0042-invoice-notes-never-in-nav-xml.md`), so
/// the printed-PDF render rejoins the regulatory NAV body with the
/// stored notes here.
#[derive(Debug, Default, Clone)]
pub struct InvoiceNotes {
    pub invoice_note: Option<String>,
    /// Per-line notes in ordinal order. `line_notes[i]` is the note
    /// for `invoice.lines[i]`; `None` for unannotated lines. Length
    /// matches `invoice.lines.len()` for invoices issued post-PR-82;
    /// pre-PR-82 invoices return an empty Vec (the column is NULL
    /// across all lines).
    pub line_notes: Vec<Option<String>>,
}

/// PR-82 — read the buyer-facing notes for `invoice_id` off the
/// tenant DuckDB. Uses `billing::load_invoice_note_in_tx` for the
/// invoice-level note and walks `invoice_line.note` in ordinal order
/// for the per-line vector.
///
/// Caller already holds the invoice id from the audit-ledger walk; we
/// open a fresh read tx here rather than thread the existing
/// `Ledger`-side connection through (the ledger crate manages its own
/// connection internally and exposes no tx handle).
///
/// Returns `InvoiceNotes::default()` (empty) if the billing tables do
/// not exist in this DB. That class of failure happens in two
/// situations: (a) a stand-alone audit-ledger-only DB (some tests
/// construct one), and (b) a hypothetical pre-PR-82 DB that never
/// passed through `DuckDbBillingStore::ensure_schema()`. Either way,
/// the invoice cannot carry notes (the columns don't exist), so an
/// empty `InvoiceNotes` is the correct read posture — not a loud-fail.
/// Real DB corruption (column missing but table present) still
/// surfaces as an error because `load_invoice_note_in_tx`'s SELECT
/// errors on that specific shape.
pub fn load_invoice_notes(db: &Path, invoice_id: &str) -> Result<InvoiceNotes> {
    let mut conn = Connection::open(db)
        .with_context(|| format!("open tenant DuckDB for notes read at {}", db.display()))?;
    let tx = conn
        .transaction()
        .context("begin read transaction for buyer-notes lookup")?;
    // Probe the `invoice` table existence cheaply. DuckDB's
    // information_schema is the portable path here; a missing table
    // returns zero rows, not an error.
    let table_present: i64 = tx
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'invoice';",
            [],
            |r| r.get(0),
        )
        .context("probe invoice table existence")?;
    if table_present == 0 {
        tx.commit().context("commit notes-read tx (no tables)")?;
        return Ok(InvoiceNotes::default());
    }
    let invoice_note = billing::load_invoice_note_in_tx(&tx, invoice_id)
        .context("billing::load_invoice_note_in_tx (print)")?;
    // Per-line notes by ordinal. We deliberately do NOT call
    // `load_ready_invoice_by_id` (which returns the full ReadyInvoice
    // including amounts) — the renderer already has line amounts from
    // the NAV XML parse, and pulling them twice would invite drift.
    let line_notes = {
        let mut stmt =
            tx.prepare("SELECT note FROM invoice_line WHERE invoice_id = ? ORDER BY ordinal ASC;")?;
        let rows = stmt.query_map([invoice_id], |r| r.get::<_, Option<String>>(0))?;
        let mut out: Vec<Option<String>> = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out
    };
    tx.commit().context("commit notes-read tx")?;
    Ok(InvoiceNotes {
        invoice_note,
        line_notes,
    })
}

// ──────────────────────────────────────────────────────────────────────
// PR-86 — per-invoice bank snapshot read
// ──────────────────────────────────────────────────────────────────────

/// PR-86 / session-111 — load the per-invoice bank snapshot off the
/// `invoice` row (PR-73's denormalised `bank_account_*` columns).
/// Returns `Ok(Some(snapshot))` when all five columns are populated
/// (post-PR-73 invoices), `Ok(None)` when the snapshot is empty
/// (pre-PR-73 invoices), and an `Err` only on real I/O / schema
/// corruption.
///
/// Same defensive posture as [`load_invoice_notes`]: a stand-alone
/// audit-ledger-only DB (where the `invoice` table doesn't exist)
/// returns `Ok(None)` so test harnesses constructing such DBs don't
/// have to scaffold a phantom snapshot.
pub fn load_invoice_bank_snapshot(
    db: &Path,
    invoice_id: &str,
) -> Result<Option<BankAccountSnapshot>> {
    let mut conn = Connection::open(db)
        .with_context(|| format!("open tenant DuckDB for bank snapshot at {}", db.display()))?;
    let tx = conn
        .transaction()
        .context("begin read transaction for bank snapshot lookup")?;
    let table_present: i64 = tx
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'invoice';",
            [],
            |r| r.get(0),
        )
        .context("probe invoice table existence")?;
    if table_present == 0 {
        tx.commit().context("commit snapshot-read tx (no tables)")?;
        return Ok(None);
    }
    let raw = load_invoice_bank_snapshot_in_tx(&tx, invoice_id)
        .context("load_invoice_bank_snapshot_in_tx (print)")?;
    tx.commit().context("commit snapshot-read tx")?;
    Ok(raw.into_typed())
}

/// PR-86 — bank fields ready for the renderer's [`PartyInfo`] block.
/// One small struct rather than a four-tuple so the field meanings
/// stay explicit at the call site in `render_to_bytes`.
struct SupplierBankFields {
    bank_account_number: Option<String>,
    iban: Option<String>,
    bank_name: Option<String>,
    swift_bic: Option<String>,
}

/// PR-86 / session-111 — resolve the supplier bank block.
///
/// Precedence:
///   1. **Per-invoice snapshot (PR-73)** when present — this is the
///      regulatory record of the bank account the operator chose at
///      issuance. The snapshot's `account_number` is rendered under
///      the canonical Hungarian `BANKSZÁMLASZÁM` label (modern HU
///      accounts are IBAN-form already, e.g. `HU71 12011375 ...`;
///      operationally this IS the IBAN). The `iban` slot stays
///      empty to avoid double-printing the same number under two
///      labels. `bank_name` and `swift_bic` flow through as the
///      secondary identifiers.
///   2. **Legacy `seller.toml` flat-root fields** as fallback for
///      pre-PR-73 invoices (snapshot is `None`). Preserves
///      byte-stable historical re-renders per ADR-0040 §addendum-C.
///
/// The brief's operator complaint — "not the swift code matters
/// which the pdf renders but the actual IBAN" — closes here. Before
/// PR-86, the renderer always read the legacy `seller.toml` flat-
/// root fields, which on a multi-bank tenant typically carried only
/// `bank_name` + `swift_bic` (no IBAN), so the printed invoice
/// surfaced the SWIFT but not the account number the buyer actually
/// needed to pay. After PR-86, the snapshot's account_number is
/// rendered as the primary pay-to line, and the SWIFT/BIC sits below
/// as the secondary identifier.
fn supplier_bank_fields(
    snapshot: Option<&BankAccountSnapshot>,
    legacy: &SellerToml,
) -> SupplierBankFields {
    if let Some(s) = snapshot {
        return SupplierBankFields {
            bank_account_number: Some(s.account_number.clone()),
            iban: None,
            bank_name: Some(s.bank_name.clone()),
            swift_bic: Some(s.swift_bic.clone()),
        };
    }
    SupplierBankFields {
        bank_account_number: legacy.bank_account_number.clone(),
        iban: legacy.iban.clone(),
        bank_name: legacy.bank_name.clone(),
        swift_bic: legacy.swift_bic.clone(),
    }
}

// ──────────────────────────────────────────────────────────────────────
// S168 — PRIVATE_PERSON PDF buyer-block re-source
// ──────────────────────────────────────────────────────────────────────

/// Buyer fields the PDF renderer needs in a single struct so the
/// override switch reads as one branch rather than a triple-shuffle of
/// local mutables in `render_to_bytes`.
struct CustomerPdfFields {
    name: String,
    address_lines: Vec<String>,
    tax_number: String,
}

/// S168 — resolve the buyer-block fields the PDF renderer ultimately
/// consumes.
///
/// **DOMESTIC / OTHER (and any path with no operator input.json):**
/// passthrough — the NAV XML on disk carries `<customerName>` and
/// `<customerAddress>` and they have been parsed into
/// [`ParsedNavInvoice`] already.
///
/// **PRIVATE_PERSON (per the operator's input.json `vat_status`):** the
/// NAV wire body suppresses `<customerName>` + `<customerAddress>` per
/// ADR-0048 amendment 2026-05-29 (NAV business rule
/// `CUSTOMER_DATA_NOT_EXPECTED`), so the parser yields empty strings /
/// empty vec for both fields. The printed PDF must still carry the
/// buyer's name + address per Áfa tv. §169 when the operator entered
/// them; we re-source from `CustomerJson` (the operator's
/// audit-immutable snapshot side-stored at issuance time). Tax number
/// is force-cleared for PRIVATE_PERSON regardless of what either source
/// carries — ADR-0048 §1 forbids it on a natural-person buyer.
fn derive_customer_pdf_fields(
    parsed: &ParsedNavInvoice,
    operator_input: Option<&CustomerJson>,
) -> CustomerPdfFields {
    let mut fields = CustomerPdfFields {
        name: parsed.customer_name.clone(),
        address_lines: parsed.customer_address_lines.clone(),
        tax_number: parsed.customer_tax_number.clone(),
    };

    if let Some(cust) = operator_input {
        if cust.vat_status == CustomerVatStatus::PrivatePerson {
            fields.name = cust.name.clone();
            fields.address_lines = cust
                .address
                .as_ref()
                .map(address_lines_from_input_json)
                .unwrap_or_default();
            fields.tax_number.clear();
        }
    }

    fields
}

/// Mirror the parser's `<customerAddress>` line layout — countryCode →
/// postalCode → city → street — and drop empty fields so the renderer
/// does not waste a row on a blank line. Matches the same order +
/// empty-skip the NAV-XML parser uses for `<common:simpleAddress>` so
/// PRIVATE_PERSON PDFs look identical in shape to DOMESTIC PDFs.
fn address_lines_from_input_json(addr: &AddressJson) -> Vec<String> {
    [
        addr.country_code.as_str(),
        addr.postal_code.as_str(),
        addr.city.as_str(),
        addr.street.as_str(),
    ]
    .iter()
    .filter(|s| !s.is_empty())
    .map(|s| (*s).to_string())
    .collect()
}

/// S168 — read the operator's `CustomerJson` from the sibling
/// `<ULID>.input.json` written at issuance (PR-47α). Returns `None`
/// when the side-store does not exist (CLI-issued invoices, pre-PR-47α
/// SPA invoices) so the renderer falls back to the NAV-XML-parsed
/// fields. Parse failure on an existing file is a loud error per
/// CLAUDE.md rule 12.
fn load_operator_customer_input(xml_path: &Path) -> Result<Option<CustomerJson>> {
    let input_json_path = sibling_input_json_path(xml_path);
    let bytes = match fs::read(&input_json_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow!(
                "read sibling input.json at {} for printed-invoice buyer override: {e}",
                input_json_path.display()
            ))
        }
    };
    let input: InvoiceInputJson = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parse sibling input.json at {} for printed-invoice buyer override",
            input_json_path.display()
        )
    })?;
    Ok(Some(input.customer))
}

// ──────────────────────────────────────────────────────────────────────
// Seller-info TOML
// ──────────────────────────────────────────────────────────────────────

/// Bank / IBAN / SWIFT fields the renderer prints under the supplier
/// party block. Sourced from `~/.aberp/<tenant>/seller.toml` (or
/// `--seller-toml <PATH>`) because the NAV wire body does NOT carry
/// these — they are on the printed invoice only per the reference
/// template (`reference_aberp_invoice_template.md`).
///
/// All four fields are optional — a tenant may not have an IBAN yet,
/// or may not need SWIFT — and the renderer hides empty rows.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct SellerToml {
    pub bank_account_number: Option<String>,
    pub iban: Option<String>,
    pub bank_name: Option<String>,
    pub swift_bic: Option<String>,
}

fn resolve_seller_toml_path(explicit_override: Option<&Path>, tenant: &str) -> Result<PathBuf> {
    if let Some(p) = explicit_override {
        return Ok(p.to_path_buf());
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow!("HOME environment variable not set; pass --seller-toml <PATH>"))?;
    Ok(PathBuf::from(home)
        .join(crate::build_profile::edition_data_dirname())
        .join(tenant)
        .join("seller.toml"))
}

/// Tiny line-oriented parser for the seller-info file. Each line is
/// either blank, a `#`-prefixed comment, a `[section]` header (ignored),
/// or `key = "value"`. No nested tables, no arrays — the field set is
/// flat. Per CLAUDE.md rule 2 + rule 13 (delete the part): a full TOML
/// parser is ~1000 LoC of dep for a 7-field config file; hand-rolling
/// the read at ~30 LoC is the surgical pick. A155 — recorded in the
/// session-57 close handoff.
pub fn read_seller_toml(path: &Path) -> Result<SellerToml> {
    if !path.exists() {
        return Err(anyhow!(
            "seller-info TOML not found at {}; create the file or pass --seller-toml <PATH>. \
             Expected shape:\n\
             [seller]\n\
             bank_account_number = \"12345678-12345678-12345678\"\n\
             iban = \"HU12 3456 ...\"\n\
             bank_name = \"OTP Bank\"\n\
             swift_bic = \"OTPVHUHB\"\n",
            path.display()
        ));
    }
    let body = fs::read_to_string(path)
        .with_context(|| format!("read seller-info TOML at {}", path.display()))?;
    parse_seller_toml(&body)
}

pub fn parse_seller_toml(body: &str) -> Result<SellerToml> {
    let mut out = SellerToml::default();
    for (lineno, raw) in body.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| {
            anyhow!(
                "seller-info TOML line {}: expected `key = \"value\"`, got `{}`",
                lineno + 1,
                line
            )
        })?;
        let key = k.trim();
        let value = v.trim().trim_matches('"').to_string();
        match key {
            "bank_account_number" => out.bank_account_number = Some(value),
            "iban" => out.iban = Some(value),
            "bank_name" => out.bank_name = Some(value),
            "swift_bic" => out.swift_bic = Some(value),
            // Silently ignore unknown keys — keeps the file extensible
            // without forcing the renderer to know every field the
            // operator might want to record. Comments + section
            // headers already filtered above.
            _ => {}
        }
    }
    Ok(out)
}

// ──────────────────────────────────────────────────────────────────────
// PR-176 — tenant-logo convention load
// ──────────────────────────────────────────────────────────────────────

/// PR-185 / Fix B — maximum logo file size on disk. Anything larger
/// is rejected at the orchestrator BEFORE the bytes hit the decoder.
/// A 50×50pt header logo is well under 100 KB in practice; 2 MiB is
/// two orders of magnitude over what an operator would supply
/// intentionally. Bump (e.g. to 4 MiB) if a real operator hits it —
/// this is a sanity cap, not a quota.
const MAX_LOGO_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// PR-176 — read the operator-supplied tenant logo from the convention
/// path. Anchored on the same directory as the seller-info TOML, so
/// the explicit `--seller-toml` override path naturally co-locates the
/// logo next to the override (test fixtures, secondary tenant
/// directories) while the default `~/.aberp/<tenant>/seller.toml`
/// keeps the logo at `~/.aberp/<tenant>/logo.png`.
///
/// PR-185 / Fix A + B — legal-document rendering must NEVER be blocked
/// by a branding asset. Every logo-path failure (missing parent,
/// over-size file, IO error, decode failure, dimension cap from
/// [`TenantLogo::from_png_bytes`]) downgrades to a `tracing::warn!`
/// and returns `Ok(None)`. The renderer then falls back to the
/// pre-PR-176 text-only header; the operator sees the WARN line on
/// the next look at the log and can re-export or remove the PNG.
/// Pre-PR-185 behavior propagated the error to a 500 from
/// `GET /api/invoices/:id/pdf` (and a non-zero exit from
/// `aberp print-invoice`) — recovery required "delete the file" with
/// no actionable error pointing at the path.
///
/// The `Result<...>` return is retained for forward compatibility; in
/// the current shape every path returns `Ok(...)` and the function is
/// effectively infallible from the caller's perspective.
fn load_tenant_logo(seller_toml_path: &Path) -> Result<Option<TenantLogo>> {
    let Some(parent) = seller_toml_path.parent() else {
        tracing::warn!(
            seller_toml = %seller_toml_path.display(),
            "seller_toml path has no parent directory — skipping tenant logo (text-only header). \
             | A seller_toml elérési útnak nincs szülőkönyvtára — címer kihagyva (csak szöveges fejléc)."
        );
        return Ok(None);
    };
    let logo_path = parent.join("logo.png");

    let metadata = match fs::metadata(&logo_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            tracing::warn!(
                logo_path = %logo_path.display(),
                error = %e,
                "tenant logo stat failed — falling back to text-only header. \
                 | Címer fájl-stat hiba — csak szöveges fejléc."
            );
            return Ok(None);
        }
    };

    let size = metadata.len();
    if size > MAX_LOGO_FILE_BYTES {
        tracing::warn!(
            logo_path = %logo_path.display(),
            size_bytes = size,
            cap_bytes = MAX_LOGO_FILE_BYTES,
            "tenant logo exceeds size cap — falling back to text-only header. \
             | A címer mérete meghaladja a méretkorlátot — csak szöveges fejléc."
        );
        return Ok(None);
    }

    let bytes = match fs::read(&logo_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                logo_path = %logo_path.display(),
                error = %e,
                "tenant logo read failed — falling back to text-only header. \
                 | Címer beolvasási hiba — csak szöveges fejléc."
            );
            return Ok(None);
        }
    };

    match TenantLogo::from_png_bytes(&bytes) {
        Ok(logo) => Ok(Some(logo)),
        Err(e) => {
            tracing::warn!(
                logo_path = %logo_path.display(),
                error = %e,
                "tenant logo PNG decode failed — falling back to text-only header. \
                 | A címer PNG dekódolása sikertelen — csak szöveges fejléc."
            );
            Ok(None)
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// S195 / PR-195 — brand primary-colour read
// ──────────────────────────────────────────────────────────────────────

/// S195 / PR-195 — read the optional `[seller.branding] primary_color`
/// from `seller.toml`, parse it via
/// [`crate::branding_config::parse_color_hex`], and return the renderer-
/// shaped `(f32, f32, f32)` RGB or `None`.
///
/// Failure-handling matches [`load_tenant_logo`]'s "legal document must
/// not be blocked by a branding asset" posture:
///   - **Missing section** → `Ok(None)` — the default-palette path.
///   - **Section present, value missing** → `Ok(None)` — same.
///   - **Malformed hex string** → `tracing::warn!` + `Ok(None)`. The
///     operator sees the WARN line on the next look at the log and
///     can fix the file; meanwhile the PDF renders with the pre-PR-195
///     ADR-0044 palette.
///   - **I/O failure on the read** → propagated as `Err(...)`. That's a
///     real configuration problem (file unreadable, permissions
///     broken) — distinct from "branding asset malformed".
fn load_brand_primary_color(seller_toml_path: &Path) -> Result<Option<(f32, f32, f32)>> {
    let cfg = match crate::branding_config::read_branding_config(seller_toml_path)? {
        Some(c) => c,
        None => return Ok(None),
    };
    match crate::branding_config::parse_color_hex(&cfg.primary_color) {
        Some(rgb) => Ok(Some(rgb)),
        None => {
            tracing::warn!(
                seller_toml = %seller_toml_path.display(),
                primary_color = %cfg.primary_color,
                "malformed [seller.branding] primary_color — falling back to default palette. \
                 Expected #RRGGBB or #RRGGBBAA. \
                 | A márka-szín formátuma érvénytelen — alapértelmezett paletta marad."
            );
            Ok(None)
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Cosmetic helpers
// ──────────────────────────────────────────────────────────────────────

/// PR-202 — map a NAV `<unitOfMeasure>` (`+ <unitOfMeasureOwn>`) pair
/// parsed off the on-disk wire body to the operator-facing Hungarian
/// label the PDF column renders.
///
/// Resolution order:
///   1. **`OWN`** with a non-empty companion → return the companion
///      verbatim (operator-typed free-text label such as `liter@15C`).
///   2. **Closed-vocab NAV token** known to
///      [`NavUnitOfMeasure::from_nav_token`] → return that variant's
///      [`NavUnitOfMeasure::display_label_hu`] (compact HU: `db`, `kg`,
///      `nap`, …).
///   3. **Unknown token** (including empty / future NAV additions, and
///      `OWN` with no/empty companion) → fall back to `"db"`. PIECE is
///      the historically-most-common unit and matches the SPA's default
///      (`NAV_UNIT_OPTIONS[0]` + `emptyProductForm().unitSelection`); a
///      sensible default keeps the column from going blank on a
///      malformed body. The pre-PR-202 hardcode also defaulted to
///      PIECE — by-product chosen here for byte-stable fallback.
fn unit_display_from_nav(unit_token: &str, own_label: Option<&str>) -> String {
    if unit_token == "OWN" {
        if let Some(label) = own_label {
            let trimmed = label.trim();
            if !trimmed.is_empty() {
                return label.to_string();
            }
        }
        return NavUnitOfMeasure::Piece.display_label_hu().to_string();
    }
    match NavUnitOfMeasure::from_nav_token(unit_token) {
        Some(token) => token.display_label_hu().to_string(),
        None => NavUnitOfMeasure::Piece.display_label_hu().to_string(),
    }
}

/// Map NAV's `<paymentMethod>` wire vocabulary to the Hungarian
/// operator-facing label the reference template uses. Unknown values
/// pass through unchanged (a future NAV code addition surfaces as a
/// readable raw string on the printed invoice rather than an opaque
/// substitution).
fn payment_method_display(wire: &str) -> String {
    match wire {
        "TRANSFER" => "Átutalás".to_string(),
        "CASH" => "Készpénz".to_string(),
        "CARD" => "Bankkártya".to_string(),
        "VOUCHER" => "Utalvány".to_string(),
        "OTHER" => "Egyéb".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seller_toml_parses_full_shape() {
        let body = r#"
[seller]
bank_account_number = "12345678-12345678-12345678"
iban = "HU12 1234 5678 9012 3456 7890"
bank_name = "OTP Bank"
swift_bic = "OTPVHUHB"
"#;
        let parsed = parse_seller_toml(body).unwrap();
        assert_eq!(
            parsed.bank_account_number.as_deref(),
            Some("12345678-12345678-12345678")
        );
        assert_eq!(
            parsed.iban.as_deref(),
            Some("HU12 1234 5678 9012 3456 7890")
        );
        assert_eq!(parsed.bank_name.as_deref(), Some("OTP Bank"));
        assert_eq!(parsed.swift_bic.as_deref(), Some("OTPVHUHB"));
    }

    #[test]
    fn seller_toml_skips_comments_and_blanks() {
        let body = "# header comment\n\n[seller]\nbank_name = \"X\"\n\n# trailing\n";
        let parsed = parse_seller_toml(body).unwrap();
        assert_eq!(parsed.bank_name.as_deref(), Some("X"));
    }

    #[test]
    fn native_to_minor_eur_two_decimals() {
        assert_eq!(native_to_minor("12.34", Currency::Eur), 1234);
        assert_eq!(native_to_minor("0.50", Currency::Eur), 50);
        assert_eq!(native_to_minor("-8636.00", Currency::Eur), -863_600);
        assert_eq!(native_to_minor("100", Currency::Eur), 10_000);
    }

    #[test]
    fn native_to_minor_huf_passthrough() {
        assert_eq!(native_to_minor("3080374", Currency::Huf), 3_080_374);
        assert_eq!(native_to_minor("-654883", Currency::Huf), -654_883);
    }

    #[test]
    fn payment_method_display_maps_transfer() {
        assert_eq!(payment_method_display("TRANSFER"), "Átutalás");
        assert_eq!(payment_method_display("CASH"), "Készpénz");
        // S160 / ADR-0050 — the operator-selectable payment method
        // (Fizetési mód) now reaches the PDF via the on-disk NAV XML; pin
        // every closed-vocab token's Hungarian label. `OTHER` → "Egyéb"
        // is NAV's catch-all (no free-text companion on the wire).
        assert_eq!(payment_method_display("CARD"), "Bankkártya");
        assert_eq!(payment_method_display("VOUCHER"), "Utalvány");
        assert_eq!(payment_method_display("OTHER"), "Egyéb");
        assert_eq!(payment_method_display("UNKNOWN"), "UNKNOWN");
    }

    /// PR-86 / session-111 — pin: when the per-invoice bank snapshot
    /// (PR-73) is present, the renderer reads its account_number as
    /// `BANKSZÁMLASZÁM`. Pre-PR-86 the renderer silently ignored the
    /// snapshot and rendered the legacy flat-root fields — closed
    /// here by `supplier_bank_fields`'s precedence rule.
    #[test]
    fn supplier_bank_fields_prefers_snapshot_when_present() {
        let snapshot = BankAccountSnapshot {
            id: "bnk_huf_raiffeisen".to_string(),
            currency: "HUF".to_string(),
            account_number: "HU71 12011375 01945291 00100002".to_string(),
            bank_name: "Raiffeisen Bank Magyarorszag".to_string(),
            swift_bic: "UBRTHUHB".to_string(),
        };
        let legacy = SellerToml {
            bank_account_number: None,
            iban: None,
            bank_name: Some("Raiffeisem".to_string()), // typo + stale
            swift_bic: Some("RAIFHU".to_string()),     // truncated + stale
        };
        let fields = supplier_bank_fields(Some(&snapshot), &legacy);

        assert_eq!(
            fields.bank_account_number.as_deref(),
            Some("HU71 12011375 01945291 00100002"),
            "snapshot account_number must render as BANKSZÁMLASZÁM",
        );
        // Snapshot path: iban slot stays empty to avoid double-printing
        // the same number under two labels.
        assert!(
            fields.iban.is_none(),
            "iban slot is empty when snapshot is present",
        );
        assert_eq!(
            fields.bank_name.as_deref(),
            Some("Raiffeisen Bank Magyarorszag"),
            "snapshot bank_name supersedes legacy `Raiffeisem` typo",
        );
        assert_eq!(
            fields.swift_bic.as_deref(),
            Some("UBRTHUHB"),
            "snapshot swift_bic supersedes legacy truncated `RAIFHU`",
        );
    }

    /// PR-86 / session-111 — pin: when no snapshot is present
    /// (pre-PR-73 invoices), the renderer falls back to legacy
    /// `seller.toml` flat-root fields verbatim. Forward-compatibility
    /// for historical re-renders.
    #[test]
    fn supplier_bank_fields_falls_back_to_legacy_when_no_snapshot() {
        let legacy = SellerToml {
            bank_account_number: Some("12345678-12345678-12345678".to_string()),
            iban: Some("HU12 3456 7890".to_string()),
            bank_name: Some("OTP Bank".to_string()),
            swift_bic: Some("OTPVHUHB".to_string()),
        };
        let fields = supplier_bank_fields(None, &legacy);

        assert_eq!(
            fields.bank_account_number.as_deref(),
            Some("12345678-12345678-12345678"),
        );
        assert_eq!(fields.iban.as_deref(), Some("HU12 3456 7890"));
        assert_eq!(fields.bank_name.as_deref(), Some("OTP Bank"));
        assert_eq!(fields.swift_bic.as_deref(), Some("OTPVHUHB"));
    }

    /// PR-86 — closing the bug Ervin caught live: the EUR snapshot
    /// (Revolut + LT IBAN) must NOT be displaced by the legacy
    /// flat-root Raiffeisen fields. Same precedence as the HUF case
    /// — the snapshot wins regardless of currency.
    #[test]
    fn supplier_bank_fields_eur_snapshot_displaces_legacy_huf_flat_root() {
        let snapshot = BankAccountSnapshot {
            id: "bnk_eur_revolut".to_string(),
            currency: "EUR".to_string(),
            account_number: "LT143250044813186860".to_string(),
            bank_name: "Revolut".to_string(),
            swift_bic: "REVOLT21".to_string(),
        };
        // Pre-PR-86 these legacy fields polluted EUR invoices too —
        // even though the EUR invoice was meant to be paid to Revolut,
        // the renderer surfaced Raiffeisem (typo) and RAIFHU (HUF).
        let legacy = SellerToml {
            bank_account_number: None,
            iban: None,
            bank_name: Some("Raiffeisem".to_string()),
            swift_bic: Some("RAIFHU".to_string()),
        };
        let fields = supplier_bank_fields(Some(&snapshot), &legacy);

        assert_eq!(
            fields.bank_account_number.as_deref(),
            Some("LT143250044813186860"),
            "EUR Revolut IBAN must render, not the stale HUF Raiffeisen legacy field",
        );
        assert_eq!(fields.bank_name.as_deref(), Some("Revolut"));
        assert_eq!(fields.swift_bic.as_deref(), Some("REVOLT21"));
    }

    /// Session-150 — the parser extracts `<customerAddress>` into
    /// `customer_address_lines` (the four simpleAddress children in
    /// document order), independent of the supplier address. Pins the
    /// PR-104 unblock: the renderer's customer party is no longer fed a
    /// hardcoded `Vec::new()`.
    #[test]
    fn parses_customer_address_lines_from_nav_xml() {
        let xml = r#"<InvoiceData xmlns:common="http://schemas.nav.gov.hu/NTCA/1.0/common">
  <invoiceNumber>TST-2024-1</invoiceNumber>
  <invoiceMain><invoice><invoiceHead>
    <supplierInfo>
      <supplierName>Eladó Kft</supplierName>
      <supplierAddress><common:simpleAddress>
        <common:countryCode>HU</common:countryCode>
        <common:postalCode>1011</common:postalCode>
        <common:city>Budapest</common:city>
        <common:additionalAddressDetail>Fő utca 1.</common:additionalAddressDetail>
      </common:simpleAddress></supplierAddress>
    </supplierInfo>
    <customerInfo>
      <customerName>Teszt Vevő Kft</customerName>
      <customerAddress><common:simpleAddress>
        <common:countryCode>HU</common:countryCode>
        <common:postalCode>1052</common:postalCode>
        <common:city>Budapest</common:city>
        <common:additionalAddressDetail>Váci utca 19.</common:additionalAddressDetail>
      </common:simpleAddress></customerAddress>
    </customerInfo>
  </invoiceHead>
  <invoiceLines><line><lineDescription>tétel</lineDescription></line></invoiceLines>
  </invoice></invoiceMain>
</InvoiceData>"#;
        let parsed = parse_nav_invoice_xml(xml.as_bytes()).expect("parse fixture XML");
        assert_eq!(parsed.customer_name, "Teszt Vevő Kft");
        assert_eq!(
            parsed.customer_address_lines,
            vec![
                "HU".to_string(),
                "1052".to_string(),
                "Budapest".to_string(),
                "Váci utca 19.".to_string(),
            ],
            "buyer address lines must parse from <customerAddress>"
        );
        // Regression guard: the supplier address still parses
        // independently and is not clobbered by the customer block.
        assert_eq!(
            parsed.supplier_address_lines,
            vec![
                "HU".to_string(),
                "1011".to_string(),
                "Budapest".to_string(),
                "Fő utca 1.".to_string(),
            ],
        );
    }

    // ── S168 — PRIVATE_PERSON PDF buyer-block re-source ───────────────

    /// Synthesise a `ParsedNavInvoice` whose customer slots mirror a
    /// PRIVATE_PERSON NAV body — `<customerName>` and `<customerAddress>`
    /// absent (Session-154), so the parser returns empty strings / empty
    /// vec. The fixture keeps the supplier + line fields valid for the
    /// downstream renderer, though the unit tests below only assert on
    /// the customer-PDF derivation so those fields are not exercised.
    fn private_person_parsed_with_empty_customer_block() -> ParsedNavInvoice {
        let mut p = ParsedNavInvoice::empty();
        p.invoice_number = "TST-2026-S168".to_string();
        p.issue_date = Date::from_calendar_date(2026, time::Month::May, 30).unwrap();
        p.lines.push(ParsedNavLine::default());
        // Customer block as Session-154 leaves it for PRIVATE_PERSON:
        // NAV XML suppressed both customerName + customerAddress, parser
        // produced empties. Tax number was already empty (forbidden).
        p
    }

    /// Synthesise a DOMESTIC `ParsedNavInvoice` — NAV XML carries name +
    /// address + structured tax number. Used to pin the passthrough
    /// branch (operator input never wins over NAV XML for non-private
    /// statuses).
    fn domestic_parsed_with_populated_customer_block() -> ParsedNavInvoice {
        let mut p = ParsedNavInvoice::empty();
        p.invoice_number = "TST-2026-S168".to_string();
        p.issue_date = Date::from_calendar_date(2026, time::Month::May, 30).unwrap();
        p.lines.push(ParsedNavLine::default());
        p.customer_name = "Domestic Kft".to_string();
        p.customer_address_lines = vec![
            "HU".to_string(),
            "1011".to_string(),
            "Budapest".to_string(),
            "Fő utca 1.".to_string(),
        ];
        p.customer_tax_number = "12345678-2-13".to_string();
        p
    }

    fn private_person_input(name: &str, address: Option<AddressJson>) -> CustomerJson {
        CustomerJson {
            vat_status: CustomerVatStatus::PrivatePerson,
            partner_id: None,
            tax_number: String::new(),
            name: name.to_string(),
            address,
        }
    }

    fn full_address() -> AddressJson {
        AddressJson {
            country_code: "HU".to_string(),
            postal_code: "1052".to_string(),
            city: "Budapest".to_string(),
            street: "Váci utca 19.".to_string(),
        }
    }

    /// S168 — table row 1: PRIVATE_PERSON buyer with name + address both
    /// present on the operator's input.json. PDF must render both —
    /// pre-fix the buyer block was empty because the NAV XML strips
    /// both fields for PRIVATE_PERSON (Session-154).
    #[test]
    fn private_person_pdf_renders_name_and_address_when_both_present() {
        let parsed = private_person_parsed_with_empty_customer_block();
        let input = private_person_input("Kovács János", Some(full_address()));
        let fields = derive_customer_pdf_fields(&parsed, Some(&input));

        assert_eq!(fields.name, "Kovács János");
        assert_eq!(
            fields.address_lines,
            vec![
                "HU".to_string(),
                "1052".to_string(),
                "Budapest".to_string(),
                "Váci utca 19.".to_string(),
            ],
        );
        assert!(
            fields.tax_number.is_empty(),
            "PRIVATE_PERSON must never render a tax number (ADR-0048 §1)"
        );
    }

    /// S168 — table row 2: PRIVATE_PERSON buyer with name only (address
    /// genuinely omitted in input.json). PDF renders the name, no
    /// address line. Pre-fix even the name was empty.
    #[test]
    fn private_person_pdf_renders_name_only_when_address_absent() {
        let parsed = private_person_parsed_with_empty_customer_block();
        let input = private_person_input("Kovács János", None);
        let fields = derive_customer_pdf_fields(&parsed, Some(&input));

        assert_eq!(fields.name, "Kovács János");
        assert!(
            fields.address_lines.is_empty(),
            "no input.json address ⇒ no PDF address line, got {:?}",
            fields.address_lines
        );
        assert!(fields.tax_number.is_empty());
    }

    /// S168 — table row 3 (unusual): PRIVATE_PERSON buyer with address
    /// but no name. PDF renders the address; the name slot stays blank.
    /// Confirms the override does not gate the address on a populated
    /// name (the two fields are independently optional under §169 when
    /// the operator chose to enter only one).
    #[test]
    fn private_person_pdf_renders_address_only_when_name_absent() {
        let parsed = private_person_parsed_with_empty_customer_block();
        let input = private_person_input("", Some(full_address()));
        let fields = derive_customer_pdf_fields(&parsed, Some(&input));

        assert!(
            fields.name.is_empty(),
            "input.json name was empty; override must not invent one"
        );
        assert_eq!(
            fields.address_lines,
            vec![
                "HU".to_string(),
                "1052".to_string(),
                "Budapest".to_string(),
                "Váci utca 19.".to_string(),
            ],
        );
        assert!(fields.tax_number.is_empty());
    }

    /// S168 — table row 4: PRIVATE_PERSON buyer with neither name nor
    /// address (genuine anonymous). PDF buyer block is empty; the
    /// renderer's `write_party` already tolerates an empty name + zero
    /// address lines (it just skips the corresponding draws).
    #[test]
    fn private_person_pdf_empty_when_neither_name_nor_address_present() {
        let parsed = private_person_parsed_with_empty_customer_block();
        let input = private_person_input("", None);
        let fields = derive_customer_pdf_fields(&parsed, Some(&input));

        assert!(fields.name.is_empty());
        assert!(fields.address_lines.is_empty());
        assert!(fields.tax_number.is_empty());
    }

    /// S168 — table row 5: tax_number must NEVER render for a
    /// PRIVATE_PERSON buyer, even if the parsed NAV body somehow still
    /// carries one (legacy ledger entry, future leak). Defense-in-depth
    /// for ADR-0048 §1's closed-vocab invariant.
    #[test]
    fn private_person_pdf_force_clears_tax_number_even_if_parsed_carried_one() {
        let mut parsed = private_person_parsed_with_empty_customer_block();
        // Pre-Session-154 wire bodies could still hold a stray tax
        // number under PRIVATE_PERSON; pin that the PDF suppresses it.
        parsed.customer_tax_number = "12345678-2-13".to_string();
        let input = private_person_input("Kovács János", Some(full_address()));
        let fields = derive_customer_pdf_fields(&parsed, Some(&input));

        assert!(
            fields.tax_number.is_empty(),
            "PRIVATE_PERSON PDF must not surface a tax number — ADR-0048 §1"
        );
    }

    /// S168 — DOMESTIC passthrough: even when an operator input.json is
    /// present and carries different name/address strings, the DOMESTIC
    /// path keeps reading from the audit-immutable NAV XML. Only the
    /// PRIVATE_PERSON branch overrides — Session-150's posture for
    /// DOMESTIC ("NAV XML is the regulatory record") is preserved.
    #[test]
    fn domestic_pdf_passes_through_nav_xml_values() {
        let parsed = domestic_parsed_with_populated_customer_block();
        let input = CustomerJson {
            vat_status: CustomerVatStatus::Domestic,
            partner_id: None,
            tax_number: "12345678-2-13".to_string(),
            name: "different name in input.json".to_string(),
            address: Some(full_address()),
        };
        let fields = derive_customer_pdf_fields(&parsed, Some(&input));

        assert_eq!(fields.name, "Domestic Kft", "NAV XML wins for DOMESTIC");
        assert_eq!(
            fields.address_lines,
            vec![
                "HU".to_string(),
                "1011".to_string(),
                "Budapest".to_string(),
                "Fő utca 1.".to_string(),
            ],
        );
        assert_eq!(fields.tax_number, "12345678-2-13");
    }

    /// S168 — CLI-issued PRIVATE_PERSON path (no input.json side-store).
    /// The override is `None` so the NAV-XML-parsed values flow through
    /// unchanged. For PRIVATE_PERSON that means empty name + address —
    /// historical behaviour, out of scope for this fix (CLI callers
    /// never had the input.json side-store).
    #[test]
    fn private_person_pdf_falls_back_to_nav_xml_when_no_input_json() {
        let parsed = private_person_parsed_with_empty_customer_block();
        let fields = derive_customer_pdf_fields(&parsed, None);

        assert!(fields.name.is_empty());
        assert!(fields.address_lines.is_empty());
        assert!(fields.tax_number.is_empty());
    }

    /// S168 — address-line mirror of the NAV-XML parser's empty-skip:
    /// blank fields on the operator's input.json are dropped rather
    /// than rendered as blank PDF rows. Matches
    /// `parses_customer_address_lines_from_nav_xml`'s shape.
    #[test]
    fn address_lines_from_input_json_drops_blank_fields() {
        let addr = AddressJson {
            country_code: "HU".to_string(),
            postal_code: String::new(),
            city: "Budapest".to_string(),
            street: String::new(),
        };
        assert_eq!(
            address_lines_from_input_json(&addr),
            vec!["HU".to_string(), "Budapest".to_string()]
        );
    }

    /// Session-150 — an invoice whose NAV XML carries no
    /// `<customerAddress>` (e.g. a pre-session-150 PrivatePerson
    /// invoice) yields empty `customer_address_lines`; the renderer
    /// then skips the (absent) buyer-address lines rather than panicking.
    #[test]
    fn customer_address_lines_empty_when_block_absent() {
        let xml = r#"<InvoiceData>
  <invoiceNumber>TST-2024-2</invoiceNumber>
  <invoiceMain><invoice><invoiceHead>
    <customerInfo><customerName>Magánszemély</customerName></customerInfo>
  </invoiceHead>
  <invoiceLines><line><lineDescription>tétel</lineDescription></line></invoiceLines>
  </invoice></invoiceMain>
</InvoiceData>"#;
        let parsed = parse_nav_invoice_xml(xml.as_bytes()).expect("parse fixture XML");
        assert_eq!(parsed.customer_name, "Magánszemély");
        assert!(
            parsed.customer_address_lines.is_empty(),
            "no <customerAddress> ⇒ empty lines, got {:?}",
            parsed.customer_address_lines
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // PR-185 / S185 — load_tenant_logo tests
    // ──────────────────────────────────────────────────────────────────

    /// Per-test tempdir (no `tempfile` dep — mirrors the ScopedTempDir
    /// pattern in `incoming_invoices.rs`). Best-effort cleanup at drop.
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
            let path = std::env::temp_dir().join(format!("aberp-s185-{label}-{pid}-{nanos}-{seq}"));
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

    /// Build a minimal valid PNG of `w`×`h` solid colour. Same shape as
    /// the synth_png helper in `crates/invoice-pdf/src/logo.rs` tests —
    /// duplicated here rather than re-exported to keep the test surface
    /// of `aberp-invoice-pdf` tight.
    fn synth_png(w: u32, h: u32, color_type: png::ColorType, pixel: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, w, h);
            encoder.set_color(color_type);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            let mut buf = Vec::with_capacity((w as usize) * (h as usize) * pixel.len());
            for _ in 0..(w as usize) * (h as usize) {
                buf.extend_from_slice(pixel);
            }
            writer.write_image_data(&buf).expect("png write data");
        }
        out
    }

    #[test]
    fn load_tenant_logo_absent_file_returns_none() {
        // Pre-existing contract — pin it. `seller_toml_path` itself need
        // NOT exist; only the directory is consulted (for the sibling
        // logo.png lookup).
        let dir = ScopedTempDir::new("logo-absent");
        let seller_toml = dir.path().join("seller.toml");
        let logo = load_tenant_logo(&seller_toml).expect("absent logo is not an error");
        assert!(
            logo.is_none(),
            "absent logo ⇒ Ok(None), got {:?}",
            logo.is_some()
        );
    }

    #[test]
    fn load_tenant_logo_valid_png_returns_some() {
        // Sanity baseline — a well-formed PNG decodes through the
        // unchanged happy path and yields `Some(TenantLogo)`.
        let dir = ScopedTempDir::new("logo-ok");
        let logo_path = dir.path().join("logo.png");
        std::fs::write(
            &logo_path,
            synth_png(4, 3, png::ColorType::Rgb, &[10, 20, 30]),
        )
        .expect("write valid logo");
        let seller_toml = dir.path().join("seller.toml");
        let logo = load_tenant_logo(&seller_toml).expect("happy path");
        let logo = logo.expect("Some");
        assert_eq!(logo.width, 4);
        assert_eq!(logo.height, 3);
    }

    #[test]
    fn load_tenant_logo_malformed_png_returns_none_not_error() {
        // PR-185 / Fix A — a malformed PNG MUST NOT propagate an error
        // to the caller. The previous behaviour returned `Err(...)`
        // which surfaced as a 500 from `GET /api/invoices/:id/pdf` and
        // a non-zero exit from `aberp print-invoice`, blocking the
        // operator's invoice over a branding asset.
        let dir = ScopedTempDir::new("logo-malformed");
        let logo_path = dir.path().join("logo.png");
        // 4 random bytes — not a PNG signature, not a valid header.
        std::fs::write(&logo_path, [0xde, 0xad, 0xbe, 0xef]).expect("write malformed logo");
        let seller_toml = dir.path().join("seller.toml");
        let logo = load_tenant_logo(&seller_toml)
            .expect("malformed logo must NOT propagate Err — fall back to text-only header");
        assert!(
            logo.is_none(),
            "malformed PNG ⇒ Ok(None), so renderer falls through to text-only header"
        );
    }

    #[test]
    fn load_tenant_logo_oversize_file_returns_none() {
        // PR-185 / Fix B — file-on-disk size cap. A 3 MiB file (over
        // the 2 MiB cap) must short-circuit BEFORE the bytes hit
        // `fs::read` / the decoder, returning Ok(None).
        let dir = ScopedTempDir::new("logo-oversize");
        let logo_path = dir.path().join("logo.png");
        let oversize = vec![0u8; (MAX_LOGO_FILE_BYTES as usize) + 1];
        std::fs::write(&logo_path, &oversize).expect("write oversize logo");
        let seller_toml = dir.path().join("seller.toml");
        let logo = load_tenant_logo(&seller_toml).expect("over-size logo falls back, not errors");
        assert!(logo.is_none(), "over-size file ⇒ Ok(None)");
    }

    #[test]
    fn load_tenant_logo_oversize_dimensions_returns_none() {
        // PR-185 / Fix B — the dimension cap inside
        // `TenantLogo::from_png_bytes` returns LogoDecode; the
        // orchestrator must catch it and degrade to Ok(None) so the
        // PDF still renders with a text-only header.
        let dir = ScopedTempDir::new("logo-oversize-dim");
        let logo_path = dir.path().join("logo.png");
        // Width just beyond MAX_LOGO_DIMENSION; encoded size stays
        // well under the file-byte cap so we exercise the decoder
        // dim check, not the disk-size check.
        let png = synth_png(
            aberp_invoice_pdf::MAX_LOGO_DIMENSION + 1,
            1,
            png::ColorType::Grayscale,
            &[0],
        );
        assert!(
            (png.len() as u64) <= MAX_LOGO_FILE_BYTES,
            "test fixture must not trip the file-byte cap (got {} bytes)",
            png.len()
        );
        std::fs::write(&logo_path, &png).expect("write oversize-dim logo");
        let seller_toml = dir.path().join("seller.toml");
        let logo = load_tenant_logo(&seller_toml).expect("over-dim logo falls back, not errors");
        assert!(
            logo.is_none(),
            "dim-cap rejection ⇒ Ok(None) at the orchestrator"
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // PR-202 — `<unitOfMeasure>` parser + display-label resolver
    // ──────────────────────────────────────────────────────────────────

    /// PR-202 — the parser captures both the closed-vocab NAV token and
    /// (when present) the `OWN` free-text companion verbatim. Pre-PR-202
    /// the parser ignored both elements; the renderer then hardcoded
    /// "PIECE" for every line, so a NAP-billed product printed as PIECE.
    #[test]
    fn parser_captures_unit_of_measure_and_own_companion() {
        let xml = r#"<InvoiceData>
  <invoiceNumber>TST-2026-1</invoiceNumber>
  <invoiceMain><invoice><invoiceHead>
    <customerInfo><customerName>X</customerName></customerInfo>
  </invoiceHead>
  <invoiceLines>
    <line>
      <lineDescription>Tanácsadás</lineDescription>
      <quantity>2</quantity>
      <unitOfMeasure>DAY</unitOfMeasure>
      <unitPrice>50000</unitPrice>
    </line>
    <line>
      <lineDescription>Üzemanyag</lineDescription>
      <quantity>10</quantity>
      <unitOfMeasure>OWN</unitOfMeasure>
      <unitOfMeasureOwn>liter@15C</unitOfMeasureOwn>
      <unitPrice>500</unitPrice>
    </line>
  </invoiceLines>
  </invoice></invoiceMain>
</InvoiceData>"#;
        let parsed = parse_nav_invoice_xml(xml.as_bytes()).expect("parse fixture XML");
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.lines[0].unit_of_measure, "DAY");
        assert!(
            parsed.lines[0].unit_of_measure_own.is_none(),
            "closed-vocab line carries no <unitOfMeasureOwn>"
        );
        assert_eq!(parsed.lines[1].unit_of_measure, "OWN");
        assert_eq!(
            parsed.lines[1].unit_of_measure_own.as_deref(),
            Some("liter@15C"),
            "OWN line's free-text companion must round-trip verbatim"
        );
    }

    /// PR-202 — closed-vocab NAV token → compact HU label. The reported
    /// bug: a NAP-billed line printed as "PIECE". Pin the load-bearing
    /// resolution.
    #[test]
    fn unit_display_resolves_closed_vocab_to_compact_hu_label() {
        assert_eq!(unit_display_from_nav("DAY", None), "nap");
        assert_eq!(unit_display_from_nav("PIECE", None), "db");
        assert_eq!(unit_display_from_nav("KILOGRAM", None), "kg");
        assert_eq!(unit_display_from_nav("HOUR", None), "óra");
        assert_eq!(unit_display_from_nav("LINEAR_METER", None), "fm");
        assert_eq!(unit_display_from_nav("CUBIC_METER", None), "m³");
    }

    /// PR-202 — `OWN` + free-text companion: render the companion
    /// verbatim. `liter@15C` is the canonical fuel-measure example.
    #[test]
    fn unit_display_own_returns_free_text_companion_verbatim() {
        assert_eq!(unit_display_from_nav("OWN", Some("liter@15C")), "liter@15C");
        assert_eq!(
            unit_display_from_nav("OWN", Some("zsák (50kg)")),
            "zsák (50kg)"
        );
    }

    /// PR-202 — `OWN` with missing / blank companion → fall back to "db"
    /// (PIECE's HU label, same default as the SPA's empty form). Keeps
    /// the column from going blank on a malformed body that emitted
    /// `<unitOfMeasure>OWN</...>` but no `<unitOfMeasureOwn>` — the NAV
    /// XSD validator rejects this at issuance, so on-disk bodies are
    /// well-formed by construction; this is defence-in-depth for a
    /// hypothetical tampered/legacy body.
    #[test]
    fn unit_display_own_without_companion_falls_back_to_db() {
        assert_eq!(unit_display_from_nav("OWN", None), "db");
        assert_eq!(unit_display_from_nav("OWN", Some("")), "db");
        assert_eq!(unit_display_from_nav("OWN", Some("   ")), "db");
    }

    /// PR-202 — unknown / empty token → fall back to "db". A future NAV
    /// schema extension that adds a new token surfaces as "db" on
    /// printed invoices issued against the pre-update binary (rather
    /// than a blank column or a panic). Empty token is the legacy /
    /// malformed-body path.
    #[test]
    fn unit_display_unknown_token_falls_back_to_db() {
        assert_eq!(unit_display_from_nav("", None), "db");
        assert_eq!(
            unit_display_from_nav("FUTURE_NAV_VARIANT", None),
            "db",
            "unknown token does not panic, does not leak the raw token to the PDF",
        );
    }

    /// PR-202 — end-to-end pin via the parser: a 2-line invoice (DAY +
    /// OWN/liter@15C) yields the two expected PDF labels after parse +
    /// resolve. Mirrors what `render_to_bytes` does end-to-end without
    /// requiring the full ledger + DuckDB scaffolding.
    #[test]
    fn parser_to_pdf_label_pipeline_yields_nap_and_own_label() {
        let xml = r#"<InvoiceData>
  <invoiceNumber>TST-2026-2</invoiceNumber>
  <invoiceMain><invoice><invoiceHead>
    <customerInfo><customerName>X</customerName></customerInfo>
  </invoiceHead>
  <invoiceLines>
    <line>
      <lineDescription>Tanácsadás</lineDescription>
      <quantity>1</quantity>
      <unitOfMeasure>DAY</unitOfMeasure>
      <unitPrice>50000</unitPrice>
    </line>
    <line>
      <lineDescription>Üzemanyag</lineDescription>
      <quantity>10</quantity>
      <unitOfMeasure>OWN</unitOfMeasure>
      <unitOfMeasureOwn>liter@15C</unitOfMeasureOwn>
      <unitPrice>500</unitPrice>
    </line>
  </invoiceLines>
  </invoice></invoiceMain>
</InvoiceData>"#;
        let parsed = parse_nav_invoice_xml(xml.as_bytes()).expect("parse fixture XML");
        let labels: Vec<String> = parsed
            .lines
            .iter()
            .map(|l| unit_display_from_nav(&l.unit_of_measure, l.unit_of_measure_own.as_deref()))
            .collect();
        assert_eq!(labels, vec!["nap".to_string(), "liter@15C".to_string()]);
    }
}
