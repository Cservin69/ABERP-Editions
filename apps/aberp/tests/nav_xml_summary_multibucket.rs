//! B3′ — ADR-0103 §3.1 (Invariant S — summary coverage), **rate-only form**
//! for the Defense line.
//!
//! `write_summary` groups lines by `vat_rate_basis_points` and emits ONE
//! `<summaryByVatRate>` per group, each carrying its own group's
//! net/vat/gross; the invoice-level totals are the sum over buckets.
//!
//! Before this fix the emitter took the bucket from `lines.first()` and summed
//! across ALL lines — so a mixed-rate invoice sent NAV one bucket carrying
//! every line's money under the first line's rate (silently wrong ÁFA). The
//! local NAV XSD validator was built to match that wrong shape and bounced a
//! correct multi-bucket body with `ChildOrderViolation`; it was corrected in
//! lock-step. Ground truth: the published NAV OSA 3.0 `invoiceData.xsd`
//! defines `SummaryNormalType/summaryByVatRate` as `maxOccurs="unbounded"`.
//!
//! DIVERGENCE FROM THE PROD-LINE FIX (ABERP.git ADR-0103): the prod bucket key
//! is the composite `(vat_rate_kind, vat_rate_basis_points)`. This edition's
//! `main` has no per-line `vat_rate_kind` (ADR-0101/0102 machinery is parked on
//! `port/vat-rate-kind-s1-machinery`, not merged), so the only live defect here
//! is the mixed-*rate* collapse and the bucket key is `basis_points` alone. The
//! prod pins that depend on `vat_rate_kind` (mixed-KIND T2, exempt-VAT T7),
//! `MixedVatRateKindsUnsupported` (T13), community-VAT (T8), and the
//! kind-consistent `vat_amount` (T6) are NOT ported — they have no defect to
//! pin on this base. See the PR description for the deferral.
//!
//! Each test names the MUTATION that must turn it red (all mutation-verified).

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, ModificationReference, NavParties,
    StornoReference, SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, RateMetadata, ReadyInvoice, SeriesCode,
    SeriesId,
};
use aberp_nav_xsd_validator::validate_invoice_data;
use rust_decimal::Decimal;
use std::str::FromStr;
use time::macros::date;
use time::OffsetDateTime;

/// An all-`Percent` line. The rate-kind port (ADR-0103 (Defense)) added
/// `vat_rate_kind` to `LineItem`; this file's pins predate it and are
/// deliberately kept on the `Percent` path so they keep testing exactly what
/// they tested before — the mixed-*RATE* B3′ bucketing. The mixed-*KIND*
/// cases live in `nav_xml_summary_mixed_kind.rs`.
fn line(desc: &str, qty: i64, unit_price: i64, bp: u16) -> LineItem {
    LineItem {
        description: desc.to_string(),
        quantity: rust_decimal::Decimal::from(qty),
        unit_price: Huf(unit_price),
        vat_rate_basis_points: bp,
        vat_rate_kind: aberp_billing::VatRateKind::Percent,
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

/// Render an invoice body and prove the (corrected) validator accepts it.
fn render(lines: Vec<LineItem>) -> String {
    let invoice = invoice_with_lines(lines);
    let xml = nav_xml::render_invoice_data(
        &invoice,
        &series(),
        &domestic_parties(),
        Currency::Huf,
        None,
    )
    .expect("emitter must succeed");
    validate_invoice_data(&xml).unwrap_or_else(|e| {
        panic!(
            "validator rejected multi-bucket body: {e}\n--- bytes ---\n{}\n--- end ---",
            String::from_utf8_lossy(&xml)
        )
    });
    String::from_utf8(xml).expect("emit is UTF-8")
}

/// Whitespace-free view so field-by-field bucket assertions do not depend on
/// the emitter's indentation.
fn compact(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

fn bucket_count(body: &str) -> usize {
    body.matches("<summaryByVatRate>").count()
}

// ── T1 — two different Percent RATES on one invoice (the live B3′ case) ────

/// ⭐ The single most important pin: 27% + 5% on ONE invoice. Two buckets,
/// each carrying ITS OWN line's net/vat/gross (NOT the sum), and the
/// invoice-level totals equal the sum over buckets. This case had ZERO
/// coverage on this line before the fix and is SPA-reachable.
///
/// MUTATION: revert `write_summary` to `lines.first()` + sum-over-all — the
/// output collapses to ONE bucket carrying 30000 net under 27%.
#[test]
fn two_percent_rates_emit_two_buckets_each_with_its_own_totals() {
    // A: 27%, net 20000, vat 5400, gross 25400.  B: 5%, net 10000, vat 500, gross 10500.
    let body = render(vec![
        line("27% line", 2, 10_000, 2700),
        line("5% line", 1, 10_000, 500),
    ]);
    let c = compact(&body);
    assert_eq!(
        bucket_count(&body),
        2,
        "one bucket per distinct rate; body:\n{body}"
    );

    // 5% bucket (sorted first: lower basis points) — full triple.
    assert!(
        c.contains(
            "<summaryByVatRate><vatRate><vatPercentage>0.05</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>10000</vatRateNetAmount><vatRateNetAmountHUF>10000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>500</vatRateVatAmount><vatRateVatAmountHUF>500</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>10500</vatRateGrossAmount><vatRateGrossAmountHUF>10500</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>"
        ),
        "5% bucket must carry its OWN totals (500 VAT, not 5900); body:\n{body}"
    );
    // 27% bucket — full triple.
    assert!(
        c.contains(
            "<summaryByVatRate><vatRate><vatPercentage>0.27</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>20000</vatRateNetAmount><vatRateNetAmountHUF>20000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>5400</vatRateVatAmount><vatRateVatAmountHUF>5400</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>25400</vatRateGrossAmount><vatRateGrossAmountHUF>25400</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>"
        ),
        "27% bucket must carry its OWN totals; body:\n{body}"
    );
    // Invoice-level totals are the sum OVER buckets.
    assert!(
        c.contains(
            "<invoiceNetAmount>30000</invoiceNetAmount><invoiceNetAmountHUF>30000</invoiceNetAmountHUF>\
             <invoiceVatAmount>5900</invoiceVatAmount><invoiceVatAmountHUF>5900</invoiceVatAmountHUF>"
        ),
        "invoice-level totals must equal the sum over buckets (5900 VAT); body:\n{body}"
    );
    assert!(
        c.contains("<invoiceGrossAmount>35900</invoiceGrossAmount><invoiceGrossAmountHUF>35900</invoiceGrossAmountHUF>"),
        "invoice gross must be 35900; body:\n{body}"
    );
}

// ── T2 — MANY LINES AT ONE RATE COLLAPSE INTO A SINGLE BUCKET ──────────────

/// The other half of Invariant S, and the half every pin above structurally
/// CANNOT carry: not just "one bucket per distinct rate" but "**only** one".
///
/// Every multi-rate pin in this file carries exactly ONE line per rate, so an
/// emitter that pushed a fresh bucket for every line — never merging — would
/// satisfy all of them: T1/T3/T4 would still see two buckets with the two
/// expected triples, T5's single-LINE invoice would still see one, and T6's
/// two-line EUR invoice would still see two. Verified by mutation: replacing
/// the bucket lookup with an unconditional push left the whole `aberp` +
/// `aberp-nav-xsd-validator` + `aberp-billing` test set GREEN.
///
/// That is not a hypothetical shape. A multi-line invoice at a single rate is
/// the commonest invoice ABERP issues, and per-line bucketing would send NAV
/// N `<summaryByVatRate>` blocks all carrying the SAME `<vatRate>` — a
/// duplicated ÁFA breakdown, not the per-rate one NAV's `SummaryNormalType`
/// asks for. It is also exactly the loop the parked
/// `port/vat-rate-kind-s1-machinery` merge must hand-resolve (it conflicts
/// here), so this pin is the guard on that resolution.
///
/// MUTATION: make the bucket lookup always miss (unconditional push) — this
/// goes red on the bucket count; nothing else in the tree does.
#[test]
fn lines_sharing_a_rate_merge_into_a_single_bucket() {
    // Three separate 27% lines: net 10000 + 5000 + 1000 = 16000,
    // per-line VAT 2700 + 1350 + 270 = 4320, gross 20320.
    let body = render(vec![
        line("27% line A", 1, 10_000, 2700),
        line("27% line B", 1, 5_000, 2700),
        line("27% line C", 1, 1_000, 2700),
    ]);
    assert_eq!(
        bucket_count(&body),
        1,
        "three lines at one rate must collapse into ONE bucket, not three; body:\n{body}"
    );
    let c = compact(&body);
    assert!(
        c.contains(
            "<summaryByVatRate><vatRate><vatPercentage>0.27</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>16000</vatRateNetAmount><vatRateNetAmountHUF>16000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>4320</vatRateVatAmount><vatRateVatAmountHUF>4320</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>20320</vatRateGrossAmount><vatRateGrossAmountHUF>20320</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>"
        ),
        "the one bucket must carry the SUM of all three lines; body:\n{body}"
    );
}

// ── T2b — three rates, and a rate carried by more than one line ────────────

/// The full live shape: 27% + 18% + 0% with TWO lines at 27%. Three buckets,
/// emitted ascending by basis points, each summing only its own lines, and
/// the invoice-level totals the sum over buckets. 0% emits a literal
/// `vatPercentage 0.00` — the pre-ADR-0101 posture on this line, where a line
/// carries only a numeric rate (see the module header's divergence note).
///
/// MUTATION: unconditional bucket push (four buckets); bucket-key collapse
/// (one bucket); sort dropped (0.27 emitted first).
#[test]
fn three_rates_with_a_repeated_rate_bucket_once_each_in_ascending_order() {
    let body = render(vec![
        line("27% line A", 1, 10_000, 2700),
        line("18% line", 1, 20_000, 1800),
        line("27% line B", 1, 5_000, 2700),
        line("0% line", 1, 7_000, 0),
    ]);
    assert_eq!(
        bucket_count(&body),
        3,
        "three distinct rates over four lines → 3 buckets; body:\n{body}"
    );
    let c = compact(&body);
    assert!(
        c.contains(
            "<summaryByVatRate><vatRate><vatPercentage>0.00</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>7000</vatRateNetAmount><vatRateNetAmountHUF>7000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>0</vatRateVatAmount><vatRateVatAmountHUF>0</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>7000</vatRateGrossAmount><vatRateGrossAmountHUF>7000</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>\
             <summaryByVatRate><vatRate><vatPercentage>0.18</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>20000</vatRateNetAmount><vatRateNetAmountHUF>20000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>3600</vatRateVatAmount><vatRateVatAmountHUF>3600</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>23600</vatRateGrossAmount><vatRateGrossAmountHUF>23600</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>\
             <summaryByVatRate><vatRate><vatPercentage>0.27</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>15000</vatRateNetAmount><vatRateNetAmountHUF>15000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>4050</vatRateVatAmount><vatRateVatAmountHUF>4050</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>19050</vatRateGrossAmount><vatRateGrossAmountHUF>19050</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>\
             <invoiceNetAmount>42000</invoiceNetAmount><invoiceNetAmountHUF>42000</invoiceNetAmountHUF>\
             <invoiceVatAmount>7650</invoiceVatAmount><invoiceVatAmountHUF>7650</invoiceVatAmountHUF>"
        ),
        "three buckets ascending, each with ITS OWN totals, invoice = sum; body:\n{body}"
    );
    assert!(
        c.contains("<invoiceGrossAmount>49650</invoiceGrossAmount><invoiceGrossAmountHUF>49650</invoiceGrossAmountHUF>"),
        "invoice gross must reconcile to 49650 (= 42000 net + 7650 VAT); body:\n{body}"
    );
}

// ── T3 — storno of a multi-rate base (the second reachable path) ───────────

/// A storno of a two-rate base emits the multi-bucket summary with NEGATED
/// per-bucket amounts. The storno render path shares `write_summary` (on the
/// negated lines), so the fix covers it automatically.
///
/// MUTATION: revert `write_summary` to `lines.first()` — one negated bucket
/// carrying the whole reversal under the first rate.
#[test]
fn storno_of_multi_rate_base_emits_negated_multi_bucket_summary() {
    let base = invoice_with_lines(vec![
        line("27% line", 2, 10_000, 2700),
        line("5% line", 1, 10_000, 500),
    ]);
    let reference = StornoReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count: 2,
    };
    let xml = nav_xml::render_storno_data(
        &base,
        &series(),
        &domestic_parties(),
        &reference,
        Currency::Huf,
        None,
    )
    .expect("storno emitter must succeed");
    validate_invoice_data(&xml).unwrap_or_else(|e| {
        panic!(
            "validator rejected multi-bucket storno: {e}\n{}",
            String::from_utf8_lossy(&xml)
        )
    });
    let body = String::from_utf8(xml).unwrap();
    let c = compact(&body);
    assert_eq!(
        bucket_count(&body),
        2,
        "storno must mirror the base's two buckets; body:\n{body}"
    );
    // Negated per-bucket VAT.
    assert!(
        c.contains("<vatRateVatAmount>-500</vatRateVatAmount>"),
        "5% bucket must be negated to -500; body:\n{body}"
    );
    assert!(
        c.contains("<vatRateVatAmount>-5400</vatRateVatAmount>"),
        "27% bucket must be negated to -5400; body:\n{body}"
    );
    assert!(
        c.contains("<invoiceVatAmount>-5900</invoiceVatAmount>"),
        "invoice-level storno VAT must be -5900; body:\n{body}"
    );
}

// ── T3b — modification, the THIRD render path through `write_summary` ──────

/// `render_modification_data` is the third caller of `write_summary`, and
/// unlike storno it does NOT negate: a modification is a full-replace body
/// carrying the corrected invoice's own amounts. T1 pins the fresh-issue
/// path and T3 the storno path; without this the modification path's
/// bucketing rests on "it's the same function" rather than on a pin, and a
/// future refactor that gave modification its own summary emitter would land
/// green. The existing modification suite does reach multi-rate bodies
/// (`issue_modification_xml_round_trip.rs`) but asserts only
/// `lineOperation`, never the summary.
///
/// MUTATION: bucket-key collapse — one bucket carrying 35000 under 27%.
#[test]
fn modification_of_a_multi_rate_invoice_emits_per_rate_buckets_unnegated() {
    let invoice = invoice_with_lines(vec![
        line("27% line A", 1, 10_000, 2700),
        line("18% line", 1, 20_000, 1800),
        line("27% line B", 1, 5_000, 2700),
    ]);
    let reference = ModificationReference {
        base_invoice_number: "INV-default/00001".to_string(),
        modification_index: 1,
        base_line_count: 3,
    };
    let xml = nav_xml::render_modification_data(
        &invoice,
        &series(),
        &domestic_parties(),
        &reference,
        Currency::Huf,
        None,
    )
    .expect("modification emitter must succeed");
    validate_invoice_data(&xml).unwrap_or_else(|e| {
        panic!(
            "validator rejected multi-bucket modification: {e}\n{}",
            String::from_utf8_lossy(&xml)
        )
    });
    let body = String::from_utf8(xml).expect("emit is UTF-8");
    let c = compact(&body);
    assert_eq!(
        bucket_count(&body),
        2,
        "two distinct rates over three lines → 2 buckets; body:\n{body}"
    );
    // 18% bucket — its own line only.
    assert!(
        c.contains(
            "<vatRate><vatPercentage>0.18</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>20000</vatRateNetAmount>"
        ) && c.contains("<vatRateVatAmount>3600</vatRateVatAmount>"),
        "18% bucket must carry 20000/3600, not the invoice total; body:\n{body}"
    );
    // 27% bucket — the two 27% lines merged, POSITIVE (full-replace, not a
    // storno negation).
    assert!(
        c.contains(
            "<vatRate><vatPercentage>0.27</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>15000</vatRateNetAmount>"
        ) && c.contains("<vatRateVatAmount>4050</vatRateVatAmount>"),
        "27% bucket must merge both 27% lines to 15000/4050, unnegated; body:\n{body}"
    );
    assert!(
        c.contains(
            "<invoiceNetAmount>35000</invoiceNetAmount>\
             <invoiceNetAmountHUF>35000</invoiceNetAmountHUF>\
             <invoiceVatAmount>7650</invoiceVatAmount>\
             <invoiceVatAmountHUF>7650</invoiceVatAmountHUF>"
        ),
        "modification invoice-level totals must be the sum over buckets; body:\n{body}"
    );
}

// ── T4 — deterministic bucket order (independent of line order) ────────────

/// Two invoices with the SAME buckets but the lines in opposite order emit
/// BYTE-IDENTICAL summaries — the buckets are a stable sort on basis points,
/// not first-appearance or `HashMap` order. This keeps the on-disk XML a
/// stable canonical record.
///
/// MUTATION: drop the stable sort (emit in first-appearance order) — the two
/// renders diverge.
#[test]
fn bucket_order_is_deterministic_regardless_of_line_order() {
    let forward = render(vec![
        line("27% line", 2, 10_000, 2700),
        line("5% line", 1, 10_000, 500),
    ]);
    let reversed = render(vec![
        line("5% line", 1, 10_000, 500),
        line("27% line", 2, 10_000, 2700),
    ]);
    // The <invoiceSummary> block must be byte-identical between the two.
    let extract = |b: &str| {
        let start = b.find("<invoiceSummary>").expect("summary present");
        let end = b.find("</invoiceSummary>").expect("summary end") + "</invoiceSummary>".len();
        b[start..end].to_string()
    };
    assert_eq!(
        extract(&forward),
        extract(&reversed),
        "summary must be identical regardless of line order"
    );
}

// ── T6 — per-bucket HUF conversion, only observable on a FOREIGN currency ──

/// This is **ADR-0037 invariant C5** — "the HUF-equivalent total on the wire
/// body equals the sum of the per-VAT-rate HUF amounts (§1.c per-VAT-rate
/// posture, NOT direct conversion of the EUR invoice total)", whose declared
/// test is "N EUR invoices with **mixed VAT rates**". C5 was **unsatisfiable**
/// on this line until B3′: a mixed-rate invoice collapsed into ONE bucket, so
/// there was no "sum of the per-VAT-rate HUF amounts" to compare against. The
/// existing EUR wire-body suite (`nav_xml_eur_body.rs`) is single-rate — every
/// line is 2700bp — so it exercises C5's degenerate one-bucket case only. This
/// is C5's first real coverage on the Defense line.
///
/// The pin the tests above structurally CANNOT carry: `huf_equivalent_for`
/// is the IDENTITY for `Currency::Huf`, so on a HUF invoice "sum the
/// per-bucket HUF" and "convert the native grand total once" are the same
/// number and neither policy can distinguish the other. `write_summary`'s
/// load-bearing ADR-0037 §1.c claim — the invoice-level `*HUF` figures are
/// the SUM of the per-bucket `*HUF`, never a fresh conversion of the native
/// grand total — is therefore observable ONLY on a non-HUF invoice.
///
/// The fixture is chosen so the two policies disagree on TWO independent
/// fields at MNB rate 356.690000 (round-half-even, ADR-0037 §1.c):
///   net   5%: 300c → 1070 HUF,  27%: 500c → 1783 HUF
///         sum = 2853   vs   grand(800c) = 2854
///   vat   5%:  15c →   54 HUF,  27%: 135c →  482 HUF
///         sum =  536   vs   grand(150c) =  535
/// They diverge in OPPOSITE directions (+1 / −1), so no constant fudge and
/// no accidental sign flip passes this pin.
///
/// MUTATION: convert the native grand totals once instead of accumulating
/// the per-bucket HUF — i.e. replace the `inv_*_huf` accumulators with
/// `huf_equivalent_for(inv_net.as_i64(), currency, rate_metadata)?` etc.
/// Goes red on `invoiceNetAmountHUF` (2854) and `invoiceVatAmountHUF` (535).
#[test]
fn per_bucket_huf_conversion_is_summed_not_reconverted_on_eur() {
    // A: 5%, net 300c, vat 15c, gross 315c.  B: 27%, net 500c, vat 135c, gross 635c.
    let invoice = invoice_with_lines(vec![
        line("5% line", 3, 100, 500),
        line("27% line", 5, 100, 2700),
    ]);
    let rate_metadata = RateMetadata {
        rate: Decimal::from_str("356.690000").unwrap(),
        source: "MNB".to_string(),
        date: date!(2026 - 05 - 08),
        // Gross stamp kept self-consistent with the fixture (950c → 3389).
        huf_equivalent_total: 3389,
    };
    let xml = nav_xml::render_invoice_data(
        &invoice,
        &series(),
        &domestic_parties(),
        Currency::Eur,
        Some(&rate_metadata),
    )
    .expect("EUR multi-bucket emitter must succeed");
    validate_invoice_data(&xml).unwrap_or_else(|e| {
        panic!(
            "validator rejected EUR multi-bucket body: {e}\n{}",
            String::from_utf8_lossy(&xml)
        )
    });
    let body = String::from_utf8(xml).expect("emit is UTF-8");
    let c = compact(&body);
    assert_eq!(bucket_count(&body), 2, "one bucket per rate; body:\n{body}");

    // Per-bucket HUF: each bucket converts its OWN native total.
    assert!(
        c.contains("<vatRateNetAmount>3.00</vatRateNetAmount><vatRateNetAmountHUF>1070</vatRateNetAmountHUF>"),
        "5% bucket net must convert its own 300c → 1070 HUF; body:\n{body}"
    );
    assert!(
        c.contains("<vatRateNetAmount>5.00</vatRateNetAmount><vatRateNetAmountHUF>1783</vatRateNetAmountHUF>"),
        "27% bucket net must convert its own 500c → 1783 HUF; body:\n{body}"
    );

    // ⭐ The divergent invoice-level figures — the actual mutation surface.
    assert!(
        c.contains("<invoiceNetAmountHUF>2853</invoiceNetAmountHUF>"),
        "invoiceNetAmountHUF must be the SUM of the per-bucket HUF (1070+1783=2853), \
         NOT a fresh conversion of the 800c grand total (which gives 2854); body:\n{body}"
    );
    assert!(
        c.contains("<invoiceVatAmountHUF>536</invoiceVatAmountHUF>"),
        "invoiceVatAmountHUF must be the SUM of the per-bucket HUF (54+482=536), \
         NOT a fresh conversion of the 150c grand total (which gives 535); body:\n{body}"
    );
    // Native invoice-level totals are unaffected by the HUF policy.
    assert!(
        c.contains("<invoiceNetAmount>8.00</invoiceNetAmount>")
            && c.contains("<invoiceVatAmount>1.50</invoiceVatAmount>"),
        "native invoice totals must still be the plain sums; body:\n{body}"
    );
}

// ── T5 — single-bucket back-compat (byte-for-byte) ─────────────────────────

/// The load-bearing back-compat pin: a single-rate invoice — every invoice
/// issued to date — still emits EXACTLY one `<summaryByVatRate>` whose triple
/// and the invoice-level totals are unchanged from the pre-fix output.
///
/// MUTATION: any change to the single-group path (this asserts the exact
/// bytes of the whole summary block for a known single-rate invoice).
#[test]
fn single_rate_invoice_emits_exactly_one_bucket_byte_identical() {
    let body = render(vec![line("27% line", 2, 10_000, 2700)]);
    assert_eq!(
        bucket_count(&body),
        1,
        "single-rate invoice must be single-bucket; body:\n{body}"
    );
    let c = compact(&body);
    assert!(
        c.contains(
            "<invoiceSummary><summaryNormal>\
             <summaryByVatRate><vatRate><vatPercentage>0.27</vatPercentage></vatRate>\
             <vatRateNetData><vatRateNetAmount>20000</vatRateNetAmount><vatRateNetAmountHUF>20000</vatRateNetAmountHUF></vatRateNetData>\
             <vatRateVatData><vatRateVatAmount>5400</vatRateVatAmount><vatRateVatAmountHUF>5400</vatRateVatAmountHUF></vatRateVatData>\
             <vatRateGrossData><vatRateGrossAmount>25400</vatRateGrossAmount><vatRateGrossAmountHUF>25400</vatRateGrossAmountHUF></vatRateGrossData>\
             </summaryByVatRate>\
             <invoiceNetAmount>20000</invoiceNetAmount><invoiceNetAmountHUF>20000</invoiceNetAmountHUF>\
             <invoiceVatAmount>5400</invoiceVatAmount><invoiceVatAmountHUF>5400</invoiceVatAmountHUF>\
             </summaryNormal>\
             <summaryGrossData><invoiceGrossAmount>25400</invoiceGrossAmount><invoiceGrossAmountHUF>25400</invoiceGrossAmountHUF></summaryGrossData>\
             </invoiceSummary>"
        ),
        "single-bucket summary must be byte-identical to the pre-fix output; body:\n{body}"
    );
}
