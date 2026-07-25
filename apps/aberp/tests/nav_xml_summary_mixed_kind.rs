//! ADR-0103 (Defense) §7 — Invariant S pins for the WIDENED bucket key.
//!
//! `nav_xml_summary_multibucket.rs` pins the mixed-*RATE* half (B3′, landed in
//! PR #25, all lines `Percent`). This file pins the half that key could not
//! see: mixed-*KIND* bucketing, after ADR-0103 §4.1 widened the summary bucket
//! key from `vat_rate_basis_points` to `(vat_rate_kind, vat_rate_basis_points)`.
//!
//! **Why the rate alone is not a sufficient key.** All four wired 0%-kinds
//! carry `vat_rate_basis_points == 0` (forced by ADR-0101 §4). Under a
//! rate-only key an `AamExempt` line and an `IntraCommunityGoods` line are
//! INDISTINGUISHABLE and collapse into one bucket — which can carry only one
//! NAV case code, silently filing one kind under the other's exemption. At 0%
//! the rate carries no information at all. `two_zero_percent_kinds_*` below is
//! that case, and it is the sharpest expression of the defect.
//!
//! **Reachability — stated honestly, because it bounds what these pins buy.**
//! `validate_invoice_preflight`'s mixed-kind guard rejects every body here, so
//! none of them is reachable through the SPA issue route, which is gated. They
//! ARE reachable through the seven ungated doors: Invariant P (universal
//! preflight) is deferred on the Defense line and on prod (ADR-0103 §6.1), so
//! a CLI door can originate a mixed-kind body that never meets the guard.
//! These pins therefore protect the emitter itself, which is where Invariant S
//! lives — exactly the reason B2 sits at the derivation rather than the gate.
//! They construct `ReadyInvoice` directly, which is that ungated path's shape.

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, RateMetadata, ReadyInvoice, SeriesCode,
    SeriesId, VatRateKind,
};
use aberp_nav_xsd_validator::validate_invoice_data;
use rust_decimal::Decimal;
use std::str::FromStr;
use time::macros::date;
use time::OffsetDateTime;

fn line(desc: &str, qty: i64, unit_price: i64, bp: u16, kind: VatRateKind) -> LineItem {
    LineItem {
        description: desc.to_string(),
        quantity: rust_decimal::Decimal::from(qty),
        unit_price: Huf(unit_price),
        vat_rate_basis_points: bp,
        vat_rate_kind: kind,
        note: None,
        unit: None,
    }
}

fn invoice_with_lines(lines: Vec<LineItem>) -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 1,
        fiscal_year: 0,
        lines,
        issue_date: OffsetDateTime::now_utc(),
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
    }
}

fn domestic_parties() -> NavParties {
    NavParties {
        supplier: SupplierInfo {
            tax_number: "24904362-2-41".to_string(),
            name: "Aben Consulting Kft".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatero koz 6".to_string(),
        },
        customer: CustomerInfo {
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("27952890-2-42".to_string()),
            community_vat_number: None,
            name: "AZ9 Services".to_string(),
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1097".to_string(),
                city: "Budapest".to_string(),
                street: "Ulloi ut 1.".to_string(),
            }),
        },
    }
}

fn series() -> SeriesCode {
    SeriesCode::new("INV-default".to_string()).unwrap()
}

fn render_with(lines: Vec<LineItem>, currency: Currency, rm: Option<&RateMetadata>) -> String {
    let invoice = invoice_with_lines(lines);
    let xml = nav_xml::render_invoice_data(&invoice, &series(), &domestic_parties(), currency, rm)
        .expect("emitter must succeed");
    validate_invoice_data(&xml).unwrap_or_else(|e| {
        panic!(
            "validator rejected mixed-kind body: {e}\n--- bytes ---\n{}\n--- end ---",
            String::from_utf8_lossy(&xml)
        )
    });
    String::from_utf8(xml).expect("emit is UTF-8")
}

fn render(lines: Vec<LineItem>) -> String {
    render_with(lines, Currency::Huf, None)
}

fn compact(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

fn bucket_count(body: &str) -> usize {
    body.matches("<summaryByVatRate>").count()
}

// ── T1 — same RATE, different KIND ────────────────────────────────────────

/// ⭐ Two lines at the SAME 2700 basis points but different kinds must split
/// into TWO buckets. A rate-only key sees one rate, emits ONE bucket, and
/// files the reverse-charge line's net as ordinary taxable turnover under
/// `<vatPercentage>0.27</vatPercentage>`. Silently wrong ÁFA of the worst
/// class.
///
/// ⚠ The reverse-charge line here carries a NON-ZERO `vat_rate_basis_points`,
/// which ADR-0101 §4 forbids at the gate. That is deliberate and it is the
/// only way this case exists: a correctly-gated non-`Percent` line carries
/// `bp == 0`, so it could never collide with a 27% line on a rate-only key in
/// the first place. The colliding state is reachable exactly through the
/// seven ungated doors (Invariant P, deferred — see this file's header), and
/// it is the same bypassed state `vat_amount_is_zero_for_non_percent_kinds_
/// even_with_nonzero_rate` pins in the billing domain. Invariant V is what
/// keeps the VAT at 0 here despite the stray 2700.
///
/// MUTATION: narrow the bucket key back to `basis_points` alone →
/// `bucket_count` becomes 1 and that single bucket carries 30000 net.
#[test]
fn same_rate_different_kind_splits_into_two_buckets() {
    // Taxable: 27%, net 20000, vat 5400, gross 25400.
    // Domestic reverse charge carrying the SAME 2700 bp: net 10000, vat 0
    // (Invariant V — the kind wins over the stray rate), gross 10000.
    let body = render(vec![
        line("27% taxable", 2, 10_000, 2700, VatRateKind::Percent),
        line(
            "§142 reverse charge, stray 27% rate",
            1,
            10_000,
            2700,
            VatRateKind::DomesticReverseCharge,
        ),
    ]);
    let c = compact(&body);

    assert_eq!(
        bucket_count(&body),
        2,
        "same rate, different kind = two buckets; body:\n{body}"
    );

    // The taxable bucket carries ONLY its own line: 20000 net, 5400 vat.
    assert!(
        c.contains(
            "<summaryByVatRate><vatRate><vatPercentage>0.27</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>20000</vatRateNetAmount><vatRateNetAmountHUF>20000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>5400</vatRateVatAmount><vatRateVatAmountHUF>5400</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>25400</vatRateGrossAmount><vatRateGrossAmountHUF>25400</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>"
        ),
        "taxable bucket must carry 20000 net / 5400 vat, NOT 30000; body:\n{body}"
    );

    // The reverse-charge bucket: boolean choice element, ZERO vat, gross==net.
    assert!(
        c.contains(
            "<summaryByVatRate><vatRate><vatDomesticReverseCharge>true</vatDomesticReverseCharge></vatRate>\
             <vatRateNetData><vatRateNetAmount>10000</vatRateNetAmount><vatRateNetAmountHUF>10000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>0</vatRateVatAmount><vatRateVatAmountHUF>0</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>10000</vatRateGrossAmount><vatRateGrossAmountHUF>10000</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>"
        ),
        "reverse-charge bucket: boolean element, 0 vat, gross==net; body:\n{body}"
    );

    // Invoice-level totals are the SUM OVER BUCKETS.
    assert!(
        c.contains("<invoiceNetAmount>30000</invoiceNetAmount><invoiceNetAmountHUF>30000</invoiceNetAmountHUF>")
            && c.contains("<invoiceVatAmount>5400</invoiceVatAmount><invoiceVatAmountHUF>5400</invoiceVatAmountHUF>")
            && c.contains("<invoiceGrossAmount>35400</invoiceGrossAmount><invoiceGrossAmountHUF>35400</invoiceGrossAmountHUF>"),
        "invoice totals must be the sum over buckets (30000/5400/35400); body:\n{body}"
    );
}

// ── T2 — two 0%-kinds, where the rate carries NO information ──────────────

/// ⭐ AAM + EUFAD37: both carry `basis_points == 0`, so a rate-only key CANNOT
/// tell them apart and collapses them into one bucket carrying one case code.
/// They must emit two buckets in two DIFFERENT NAV categories:
/// `vatExemption`/`AAM` and `vatOutOfScope`/`EUFAD37` — an exemption and an
/// out-of-scope supply are not the same thing to NAV.
///
/// MUTATION: narrow the key to `basis_points` → ONE bucket; whichever kind
/// sorts first swallows the other's net under its own case code.
#[test]
fn two_zero_percent_kinds_emit_two_buckets_in_different_categories() {
    let body = render(vec![
        line("AAM exempt", 2, 10_000, 0, VatRateKind::AamExempt),
        line(
            "EUFAD37 service",
            1,
            10_000,
            0,
            VatRateKind::IntraCommunityServiceReverse,
        ),
    ]);
    let c = compact(&body);

    assert_eq!(
        bucket_count(&body),
        2,
        "two 0%-kinds must NOT collapse — the rate cannot distinguish them; body:\n{body}"
    );

    assert!(
        c.contains("<summaryByVatRate><vatRate><vatExemption><case>AAM</case>"),
        "AAM must emit vatExemption/case AAM; body:\n{body}"
    );
    assert!(
        c.contains("<summaryByVatRate><vatRate><vatOutOfScope><case>EUFAD37</case>"),
        "EUFAD37 must emit vatOutOfScope (NOT an exemption); body:\n{body}"
    );

    // Each bucket keeps its own net: 20000 and 10000, never 30000 in one.
    assert!(
        c.contains("<vatRateNetAmount>20000</vatRateNetAmount>")
            && c.contains("<vatRateNetAmount>10000</vatRateNetAmount>"),
        "each 0%-bucket keeps its OWN net (20000 / 10000); body:\n{body}"
    );
    assert!(
        !c.contains("<vatRateNetAmount>30000</vatRateNetAmount>"),
        "no bucket may carry BOTH lines' net — that is the collapse; body:\n{body}"
    );
    // Whole invoice is 0%: no VAT anywhere.
    assert!(
        c.contains("<invoiceVatAmount>0</invoiceVatAmount>"),
        "an all-0% invoice has zero VAT; body:\n{body}"
    );
}

/// The other exemption pair: AAM and KBAET are BOTH `vatExemption`, and are
/// distinguished only by their `<case>`. A rate-only key would merge them and
/// file an intra-Community supply under a domestic subject-exemption — the
/// same wrongness as T2 but harder to spot, since the outer element matches.
///
/// MUTATION: narrow the key to `basis_points` → one `vatExemption` bucket.
#[test]
fn two_exemption_kinds_keep_distinct_case_codes() {
    let body = render(vec![
        line("AAM exempt", 1, 10_000, 0, VatRateKind::AamExempt),
        line(
            "KBAET goods",
            1,
            25_000,
            0,
            VatRateKind::IntraCommunityGoods,
        ),
    ]);
    let c = compact(&body);

    assert_eq!(bucket_count(&body), 2, "AAM != KBAET; body:\n{body}");
    assert!(
        c.contains("<vatExemption><case>AAM</case>")
            && c.contains("<vatExemption><case>KBAET</case>"),
        "both case codes must survive as separate buckets; body:\n{body}"
    );
    assert!(
        c.contains("<vatRateNetAmount>10000</vatRateNetAmount>")
            && c.contains("<vatRateNetAmount>25000</vatRateNetAmount>")
            && !c.contains("<vatRateNetAmount>35000</vatRateNetAmount>"),
        "each case keeps its own net; body:\n{body}"
    );
}

/// All four wired kinds on one invoice, each mapping to its own category —
/// the full ADR-0101 §4 vocabulary exercised through the SUMMARY, in one body.
///
/// MUTATION: narrow the key to `basis_points` → all four collapse to ONE
/// bucket (they all carry rate 0).
#[test]
fn all_four_zero_percent_kinds_each_emit_their_own_category() {
    let body = render(vec![
        line("AAM", 1, 1_000, 0, VatRateKind::AamExempt),
        line("KBAET", 1, 2_000, 0, VatRateKind::IntraCommunityGoods),
        line(
            "EUFAD37",
            1,
            4_000,
            0,
            VatRateKind::IntraCommunityServiceReverse,
        ),
        line("§142 DRC", 1, 8_000, 0, VatRateKind::DomesticReverseCharge),
    ]);
    let c = compact(&body);

    assert_eq!(
        bucket_count(&body),
        4,
        "four distinct kinds at the same 0% rate = four buckets; body:\n{body}"
    );
    assert!(
        c.contains("<vatExemption><case>AAM</case>"),
        "AAM; body:\n{body}"
    );
    assert!(
        c.contains("<vatExemption><case>KBAET</case>"),
        "KBAET; body:\n{body}"
    );
    assert!(
        c.contains("<vatOutOfScope><case>EUFAD37</case>"),
        "EUFAD37 -> vatOutOfScope; body:\n{body}"
    );
    assert!(
        c.contains("<vatDomesticReverseCharge>true</vatDomesticReverseCharge>"),
        "§142 -> boolean vatDomesticReverseCharge; body:\n{body}"
    );
    // Net is partitioned exactly: 1000/2000/4000/8000, summing to 15000.
    for n in ["1000", "2000", "4000", "8000"] {
        assert!(
            c.contains(&format!("<vatRateNetAmount>{n}</vatRateNetAmount>")),
            "bucket net {n} must appear exactly as its own; body:\n{body}"
        );
    }
    assert!(
        c.contains("<invoiceNetAmount>15000</invoiceNetAmount>")
            && c.contains("<invoiceVatAmount>0</invoiceVatAmount>"),
        "invoice net is the sum over buckets, VAT is zero; body:\n{body}"
    );
}

// ── T6 — determinism across line permutations, at ONE rate ────────────────

/// Buckets must sort on `(kind, basis_points)`. Sorting on the rate alone
/// leaves two same-rate buckets in LINE order, so permuting the lines would
/// permute the emitted bytes — and the on-disk XML is the canonical record of
/// what NAV saw, so two renders of one invoice must not differ.
///
/// MUTATION: drop the `kind.as_str()` term from the sort (leaving
/// `sort_by_key(|b| b.basis_points)`) → the two orderings diverge.
#[test]
fn bucket_order_is_deterministic_across_same_rate_kinds() {
    let a = line("AAM", 1, 10_000, 0, VatRateKind::AamExempt);
    let b = line("KBAET", 1, 20_000, 0, VatRateKind::IntraCommunityGoods);

    let forward = render(vec![a.clone(), b.clone()]);
    let reversed = render(vec![b, a]);

    // Strip the per-render ULID/timestamp-bearing head so the comparison is of
    // the SUMMARY bytes, which is what the sort governs.
    let summary_of = |s: &str| {
        let start = s.find("<invoiceSummary>").expect("summary present");
        s[start..].to_string()
    };
    assert_eq!(
        summary_of(&forward),
        summary_of(&reversed),
        "summary bytes must not depend on line order"
    );
}

// ── T10 — per-bucket HUF on a mixed-KIND EUR invoice (ADR-0037 §1.c) ──────

/// ADR-0037 §1.c: each bucket converts its OWN native total and the
/// invoice-level HUF figures are the SUM of the per-bucket HUF figures — never
/// a fresh conversion of the native grand total.
///
/// ⚠ Fixture constraint: `huf_equivalent_for` is the IDENTITY for
/// `Currency::Huf`, so a HUF fixture cannot pin ANY conversion behaviour —
/// "sum the per-bucket HUF" and "convert the grand total once" are the same
/// number there. This uses EUR + `RateMetadata`.
///
/// Fixture (MNB rate 356.690000, round-half-even), reusing the amounts the
/// mixed-RATE sibling pin proved divergent, but split by KIND instead:
///   AAM  300c → 1070 HUF,  KBAET 500c → 1783 HUF
///   sum = 2853   vs   grand(800c) = 2854
/// Both kinds are 0%, so the split here is driven ENTIRELY by the kind term
/// of the bucket key — under a rate-only key there is one bucket and the
/// per-bucket-vs-grand-total distinction disappears with it.
///
/// MUTATION: convert the grand total once instead of accumulating per-bucket
/// HUF → `invoiceNetAmountHUF` becomes 2854.
#[test]
fn per_bucket_huf_is_summed_not_reconverted_on_mixed_kind_eur() {
    let rate_metadata = RateMetadata {
        rate: Decimal::from_str("356.690000").unwrap(),
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        // Gross stamp kept self-consistent with the fixture (800c → 2854).
        huf_equivalent_total: 2854,
    };

    let body = render_with(
        vec![
            line("AAM", 3, 100, 0, VatRateKind::AamExempt),
            line("KBAET", 5, 100, 0, VatRateKind::IntraCommunityGoods),
        ],
        Currency::Eur,
        Some(&rate_metadata),
    );
    let c = compact(&body);

    assert_eq!(
        bucket_count(&body),
        2,
        "the split here is KIND-driven — both lines are 0%; body:\n{body}"
    );

    // Per-bucket HUF: each bucket converts its OWN native total.
    assert!(
        c.contains("<vatRateNetAmount>3.00</vatRateNetAmount><vatRateNetAmountHUF>1070</vatRateNetAmountHUF>"),
        "AAM bucket must convert its own 300c → 1070 HUF; body:\n{body}"
    );
    assert!(
        c.contains("<vatRateNetAmount>5.00</vatRateNetAmount><vatRateNetAmountHUF>1783</vatRateNetAmountHUF>"),
        "KBAET bucket must convert its own 500c → 1783 HUF; body:\n{body}"
    );

    // ⭐ The divergent invoice-level figure — the mutation surface.
    assert!(
        c.contains("<invoiceNetAmountHUF>2853</invoiceNetAmountHUF>"),
        "invoiceNetAmountHUF must be the SUM of the per-bucket HUF \
         (1070+1783=2853), NOT a fresh conversion of the 800c grand total \
         (which gives 2854); body:\n{body}"
    );
}
