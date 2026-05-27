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
use aberp_billing::{self as billing, Currency, RateMetadata};
use aberp_invoice_pdf::{render_invoice, InvoiceModel, LineItem as PdfLine, PartyInfo};
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

    // 6. Build the renderer model.
    let supplier = PartyInfo {
        name: parsed.supplier_name.clone(),
        address_lines: parsed.supplier_address_lines.clone(),
        tax_number: parsed.supplier_tax_number.clone(),
        bank_account_number: seller_info.bank_account_number.clone(),
        iban: seller_info.iban.clone(),
        bank_name: seller_info.bank_name.clone(),
        swift_bic: seller_info.swift_bic.clone(),
    };
    let customer = PartyInfo {
        name: parsed.customer_name.clone(),
        address_lines: Vec::new(),
        tax_number: parsed.customer_tax_number.clone(),
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
            unit: "PIECE".to_string(),
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
            lines: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ParsedNavLine {
    pub description: String,
    pub quantity: u32,
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
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(local_name(e.name().as_ref())).to_string();
                if name == "line" && cur_line.is_some() {
                    out.lines.push(cur_line.take().unwrap());
                }
                if name == "supplierAddress" && address_in.is_some() {
                    out.supplier_address_lines = std::mem::take(&mut address_buf);
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
                    &date_fmt,
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
            "countryCode" | "postalCode" | "city" | "additionalAddressDetail" => {
                if !value.is_empty() {
                    address_buf.push(value.to_string());
                }
            }
            _ => {}
        }
    }

    // ── Per-line fields ──────────────────────────────────────────────
    if let Some(line) = cur_line.as_mut() {
        match last {
            "lineDescription" => line.description = value.to_string(),
            "quantity" => {
                line.quantity = value
                    .parse::<u32>()
                    .or_else(|_| {
                        // NAV permits decimal quantities; we truncate
                        // to an integer for the printed quantity since
                        // the renderer's LineItem::quantity is u32. A
                        // future PR-44ε.UI / typed-quantity lift would
                        // be the trigger to widen this.
                        value
                            .split_once('.')
                            .map(|(w, _)| w)
                            .unwrap_or(value)
                            .parse::<u32>()
                    })
                    .map_err(|e| anyhow!("<quantity> `{value}` parse: {e}"))?;
            }
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
        .join(".aberp")
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
// Cosmetic helpers
// ──────────────────────────────────────────────────────────────────────

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
        assert_eq!(payment_method_display("UNKNOWN"), "UNKNOWN");
    }
}
