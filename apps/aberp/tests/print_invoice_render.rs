//! Integration tests for the `aberp print-invoice` orchestrator
//! (PR-44ε.1 / A152). Builds the audit-ledger entries + on-disk NAV
//! XML the orchestrator consumes, calls `print_invoice::run`, and
//! re-parses the rendered PDF to assert §1.a-compliant content.
//!
//! Four pin tests, mirroring the brief's success criteria:
//!
//! 1. **EUR invoice** — text contains `EUR`, `Árfolyam`, `Ft` (proves
//!    the HUF-equivalent surface fired), seller ADÓSZÁM, buyer
//!    ADÓSZÁM, and the invoice total amounts.
//! 2. **HUF invoice** — text contains `Ft` (the HUF native suffix) but
//!    NOT `Árfolyam:` (no EUR-only rate line) and NOT `EUR` (the
//!    currency code).
//! 3. **Round-half-even differential** — a rate × cents combination
//!    where the naive half-up integer differs from the round-half-even
//!    integer; the rendered text carries the round-half-even integer.
//! 4. **Single-page** — the PDF's `Pages.Count` is exactly `1`.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::{
    Currency, CustomerId, Huf, IdempotencyKey, InvoiceId, LineItem, RateMetadata, ReadyInvoice,
    SeriesCode, SeriesId,
};
use lopdf::Object;
use rust_decimal::Decimal;
use time::macros::date;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp::audit_payloads::InvoiceDraftCreatedPayload;
use aberp::cli::PrintInvoiceArgs;
use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
use aberp::print_invoice;

// ──────────────────────────────────────────────────────────────────────
// Test scaffolding
// ──────────────────────────────────────────────────────────────────────

const TEST_TENANT: &str = "print_invoice_test";

/// Allocate a unique temp dir under the system temp root. We avoid
/// `tempfile::TempDir` to keep the dev-dep surface tight (per CLAUDE.md
/// rule 13); the per-test ULID directory is leaked at end-of-test
/// which is acceptable for the OS-temp-root posture this repo uses.
fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-print-invoice")
        .join(format!("{}-{}", label, Ulid::new()));
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn fixture_seller_toml(dir: &Path) -> PathBuf {
    let p = dir.join("seller.toml");
    fs::write(
        &p,
        r#"[seller]
bank_account_number = "12345678-12345678-12345678"
iban = "HU12 1234 5678 9012 3456 7890"
bank_name = "OTP Bank Nyrt."
swift_bic = "OTPVHUHB"
"#,
    )
    .expect("write seller.toml");
    p
}

fn fixture_parties() -> NavParties {
    NavParties {
        supplier: SupplierInfo {
            tax_number: "12345678-1-42".to_string(),
            name: "ABERP Test Kft.".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1234".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Test utca 1.".to_string(),
        },
        customer: CustomerInfo {
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit
            // Domestic posture for legacy test fixtures.
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("87654321-2-13".to_string()),
            name: "Vevő Kft.".to_string(),
            // PR-77 / session-101 — `customerAddress` required for any
            // DOMESTIC customerVatStatus.
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Váci utca 19.".to_string(),
            }),
        },
    }
}

/// Build a ReadyInvoice with one line at the given unit price + quantity
/// + VAT-rate-basis-points. Helper for both HUF (Huf(forints)) and EUR
/// (Huf(cents)) — the underlying field is `i64` regardless per PR-44γ's
/// interim posture.
fn fixture_ready_invoice(
    unit_price_minor: i64,
    quantity: rust_decimal::Decimal,
    vat_bps: u16,
) -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        lines: vec![LineItem {
            description: "Test megnevezés (időszak árú szolgáltatás)".to_string(),
            quantity,
            unit_price: Huf(unit_price_minor),
            vat_rate_basis_points: vat_bps,
            note: None,
            unit: None,
        }],
        issue_date: OffsetDateTime::now_utc(),
        // PR-84 — fixture defaults both date fields to issue date.
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
        sequence_number: 7,
        fiscal_year: 0,
    }
}

/// Wire the audit ledger + on-disk XML for one invoice id under a
/// fresh tenant DB. Returns the InvoiceId prefixed string the
/// orchestrator looks up plus the temp-dir holder.
struct WiredInvoice {
    dir: PathBuf,
    db_path: PathBuf,
    invoice_id: String,
}

fn wire_invoice(
    label: &str,
    invoice: &ReadyInvoice,
    series_code: &SeriesCode,
    parties: &NavParties,
    currency: Currency,
    rate_metadata: Option<&RateMetadata>,
) -> WiredInvoice {
    let dir = test_dir(label);
    let db_path = dir.join("aberp.duckdb");
    let xml_path = dir.join("invoice.xml");

    // Render real NAV InvoiceData XML — exercises the same render path
    // a live `aberp issue-invoice` run would write to disk.
    let xml = nav_xml::render_invoice_data(invoice, series_code, parties, currency, rate_metadata)
        .expect("render NAV XML");
    fs::write(&xml_path, &xml).expect("write NAV XML");

    // Open a Ledger (creates the schema on first touch) and append
    // the InvoiceDraftCreated entry the orchestrator looks up.
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    let mut ledger = Ledger::open(&db_path, tenant.clone(), binary_hash).expect("open ledger");

    let idempotency_key = IdempotencyKey::new();
    let payload = if let Some(rate) = rate_metadata {
        InvoiceDraftCreatedPayload::from_invoice_with_rate(
            invoice,
            idempotency_key,
            Some(xml_path.clone()),
            currency,
            rate,
        )
    } else {
        InvoiceDraftCreatedPayload::from_invoice_with_xml_path(
            invoice,
            idempotency_key,
            xml_path.clone(),
        )
    };
    let actor = Actor::from_local_cli("test-session".to_string(), "test-user");
    ledger
        .append(
            EventKind::InvoiceDraftCreated,
            payload.to_bytes(),
            actor,
            Some(idempotency_key.to_canonical_string()),
        )
        .expect("append InvoiceDraftCreated");

    let _ = xml_path;
    WiredInvoice {
        invoice_id: invoice.id.to_prefixed_string(),
        dir,
        db_path,
    }
}

fn run_print_invoice(wired: &WiredInvoice) -> PathBuf {
    let seller_toml = fixture_seller_toml(&wired.dir);
    let out = wired.dir.join("invoice.pdf");
    let args = PrintInvoiceArgs {
        id: wired.invoice_id.clone(),
        out: out.clone(),
        db: wired.db_path.clone(),
        tenant: TEST_TENANT.to_string(),
        seller_toml: Some(seller_toml),
    };
    print_invoice::run(&args).expect("print_invoice::run");
    out
}

fn read_pdf_text(path: &Path) -> String {
    let bytes = fs::read(path).expect("read PDF");
    pdf_extract::extract_text_from_mem(&bytes).expect("pdf-extract")
}

fn pdf_page_count(path: &Path) -> u32 {
    let bytes = fs::read(path).expect("read PDF");
    let doc = lopdf::Document::load_mem(&bytes).expect("lopdf load_mem");
    // Walk the catalog for `/Pages → /Count`.
    let catalog_id = doc
        .trailer
        .get(b"Root")
        .and_then(|o| match o {
            Object::Reference(id) => Ok(*id),
            _ => Err(lopdf::Error::ObjectNotFound),
        })
        .expect("catalog reference");
    let catalog = doc.get_object(catalog_id).expect("catalog object");
    let catalog_dict = catalog.as_dict().expect("catalog dict");
    let pages_ref = catalog_dict.get(b"Pages").expect("Pages ref");
    let pages_id = match pages_ref {
        Object::Reference(id) => *id,
        _ => panic!("Pages not a reference"),
    };
    let pages = doc.get_object(pages_id).expect("Pages object");
    let pages_dict = pages.as_dict().expect("Pages dict");
    let count = pages_dict.get(b"Count").expect("Count");
    match count {
        Object::Integer(n) => *n as u32,
        _ => panic!("Count not integer"),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pin tests
// ──────────────────────────────────────────────────────────────────────

/// EUR invoice — the printed PDF text contains the §1.a-mandated
/// fields: `EUR` (currency), `Árfolyam` (rate line), `Ft` (HUF suffix
/// on the HUF-equivalent totals), and both party ADÓSZÁM values.
#[test]
fn eur_invoice_renders_with_arfolyam_and_huf_totals() {
    // 100 cents × 1 unit × 27% VAT = net 100c / vat 27c / gross 127c.
    let invoice = fixture_ready_invoice(100, rust_decimal::Decimal::from(1), 2700);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = fixture_parties();
    let rate = RateMetadata {
        rate: Decimal::from_str("356.69").unwrap(),
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        huf_equivalent_total: 453, // 127c × 3.5669 round-half-even ≈ 453 Ft
    };
    let wired = wire_invoice(
        "eur",
        &invoice,
        &series,
        &parties,
        Currency::Eur,
        Some(&rate),
    );
    let pdf = run_print_invoice(&wired);
    let text = read_pdf_text(&pdf);

    assert!(
        text.contains("EUR"),
        "expected EUR currency in PDF text:\n{text}"
    );
    assert!(
        text.contains("Árfolyam") || text.contains("rfolyam"),
        "expected Árfolyam rate line in PDF text:\n{text}"
    );
    assert!(
        text.contains("Ft"),
        "expected Ft suffix in PDF text:\n{text}"
    );
    assert!(
        text.contains("12345678-1-42"),
        "expected seller ADÓSZÁM in PDF text:\n{text}"
    );
    assert!(
        text.contains("87654321-2-13"),
        "expected buyer ADÓSZÁM in PDF text:\n{text}"
    );
}

/// S157 — a decimal line quantity renders on the printed PDF with the
/// Hungarian comma (`1,5`), NOT truncated to `1` and NOT dot-separated.
#[test]
fn decimal_quantity_renders_with_hungarian_comma() {
    // 1000 HUF × 1.5 units = net 1500.
    let invoice = fixture_ready_invoice(1000, Decimal::new(15, 1), 2700);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = fixture_parties();
    let wired = wire_invoice("qty-dec", &invoice, &series, &parties, Currency::Huf, None);
    let pdf = run_print_invoice(&wired);
    let text = read_pdf_text(&pdf);

    assert!(
        text.contains("1,5"),
        "expected decimal quantity '1,5' (Hungarian comma) in PDF text:\n{text}"
    );
    assert!(
        !text.contains("1.5"),
        "quantity must use the Hungarian comma, not a dot:\n{text}"
    );
}

/// HUF invoice — the printed PDF text contains `Ft` but NOT `Árfolyam:`
/// (the EUR-only rate line) and NOT `EUR` (the currency code).
#[test]
fn huf_invoice_renders_without_arfolyam_line() {
    // 1000 HUF × 1 unit × 27% VAT = net 1000 / vat 270 / gross 1270.
    let invoice = fixture_ready_invoice(1000, rust_decimal::Decimal::from(1), 2700);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = fixture_parties();
    let wired = wire_invoice("huf", &invoice, &series, &parties, Currency::Huf, None);
    let pdf = run_print_invoice(&wired);
    let text = read_pdf_text(&pdf);

    assert!(
        text.contains("Ft"),
        "expected Ft suffix in PDF text:\n{text}"
    );
    assert!(
        !text.contains("Árfolyam:"),
        "HUF PDF must NOT carry the Árfolyam: rate line:\n{text}"
    );
    assert!(
        !text.contains("EUR"),
        "HUF PDF must NOT mention EUR currency:\n{text}"
    );
    assert!(
        text.contains("12345678-1-42"),
        "expected seller ADÓSZÁM in PDF text:\n{text}"
    );
}

/// Round-half-even differential — pick an amount × rate combination
/// whose half-up integer differs from the round-half-even integer.
///
/// `1250 cents × 1.0 HUF/EUR = 12.50 HUF`:
///   - Round-half-even: `12` (even forint).
///   - Naive half-up:   `13`.
///
/// The renderer per-VAT-rate path runs `huf_equivalent_round_half_even`
/// against `vat_minor=1250c` (with the rate stamped at 1.0 here so the
/// tie surfaces cleanly). The rendered HUF text MUST carry `12`, not
/// `13`. A regression that flips the rounding mode would surface as a
/// `13 Ft` string in the rendered text.
#[test]
fn eur_invoice_uses_round_half_even_for_per_rate_huf() {
    // Construct an EUR invoice whose VAT in cents lands exactly at a
    // half-cent-equivalent HUF tie under rate=1.0:
    //   - unit_price = 1000 cents, qty=1, vat_rate=125 bps (1.25%).
    //   - net = 1000c; vat = 1000 × 125 / 10000 = 12c (floor — see
    //     LineItem::vat_amount). That's no tie. Try a different pair.
    //
    // Easier: feed `vat_minor = 1250` directly via a custom line — set
    // unit_price=10000 cents, qty=1, vat_rate=1250 bps (12.5%). Then
    // net=10000c, vat = 10000 × 1250 / 10000 = 1250c. ✓
    //
    // At rate=1.0: vat HUF = 12.50 → round-half-even = 12 (even forint);
    // half-up = 13.
    let invoice = fixture_ready_invoice(10_000, rust_decimal::Decimal::from(1), 1250);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = fixture_parties();
    let rate = RateMetadata {
        rate: Decimal::from_str("1.0").unwrap(),
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        // huf_equivalent_total for the invoice gross (10000 + 1250 = 11250c)
        // at rate 1.0 = 112.50 → round-half-even = 112.
        huf_equivalent_total: 112,
    };
    let wired = wire_invoice(
        "round-half-even",
        &invoice,
        &series,
        &parties,
        Currency::Eur,
        Some(&rate),
    );
    let pdf = run_print_invoice(&wired);
    let text = read_pdf_text(&pdf);

    // The per-VAT-rate HUF row prints `12 Ft` (round-half-even on the
    // 1250c × 1.0 = 12.50 tie). A half-up regression would print
    // `13 Ft`. Pin the differential.
    assert!(
        text.contains("12 Ft") && !text.contains("13 Ft"),
        "round-half-even pin failed — expected `12 Ft` and NOT `13 Ft` in PDF text:\n{text}"
    );
}

/// Single-page assertion — the printed invoice fits on one A4 page per
/// the renderer's deliberate posture (`Pages.Count = 1` in
/// `crates/invoice-pdf/src/lib.rs::render_invoice`). A regression that
/// adds an overflow second page would surface here.
#[test]
fn printed_invoice_pdf_is_single_page() {
    let invoice = fixture_ready_invoice(1000, rust_decimal::Decimal::from(1), 2700);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let parties = fixture_parties();
    let wired = wire_invoice(
        "single-page",
        &invoice,
        &series,
        &parties,
        Currency::Huf,
        None,
    );
    let pdf = run_print_invoice(&wired);
    let count = pdf_page_count(&pdf);
    assert_eq!(count, 1, "printed invoice must be exactly one A4 page");
}
