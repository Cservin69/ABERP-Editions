//! ADR-0101 Session 1 — NAV emit round-trip per `VatRateKind`.
//!
//! Test group A/B/F of the ADR §8 plan:
//!   * A — each fully-wired kind renders the correct `<lineVatRate>` choice
//!     element with the **Ervin-confirmed** case code (AAM / KBAET /
//!     EUFAD37 / the `vatDomesticReverseCharge` boolean), the matching
//!     `summaryByVatRate` mirror agrees, and the whole body passes the
//!     local NAV XSD validator. Plus the negative pins (an AAM line emits
//!     no `<vatPercentage>`; a reverse-charge line emits no
//!     `<vatExemption>`).
//!   * B — backward-compat: a `Percent`/0% line emits `<vatPercentage>0.00`
//!     byte-for-byte (the pre-0101 output) and NONE of the new choice
//!     elements. This test is impossible to pass if the default emit path
//!     changed (CLAUDE.md rule 9 / ADR-0101 §5.4).
//!   * F — the validator now accepts `vatDomesticReverseCharge` and the
//!     `case`+`reason` children, and rejects a malformed exemption.
//!
//! The line/summary mirror agreement (§3.4) is load-bearing: a line and
//! its `summaryByVatRate` bucket in different NAV categories is itself a
//! cross-field NAV rejection.

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, ReadyInvoice, SeriesCode, SeriesId, VatRateKind,
};
use aberp_nav_xsd_validator::validate_invoice_data;
use time::OffsetDateTime;

/// One-line invoice carrying `kind`. Exempt / reverse-charge kinds carry a
/// 0% numeric rate (their line VAT amount is 0); the `Percent` case keeps
/// the passed basis points.
fn invoice_with_kind(kind: VatRateKind, vat_rate_basis_points: u16) -> ReadyInvoice {
    ReadyInvoice {
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        sequence_number: 1,
        fiscal_year: 0,
        lines: vec![LineItem {
            description: "ADR-0101 line".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(10_000),
            vat_rate_basis_points,
            vat_rate_kind: kind,
            note: None,
            unit: None,
        }],
        issue_date: OffsetDateTime::now_utc(),
        payment_deadline: OffsetDateTime::now_utc().date(),
        delivery_date: OffsetDateTime::now_utc().date(),
    }
}

/// Domestic buyer — kept constant across the kind matrix. The buyer VAT
/// status is independent of the per-LINE VAT rate-kind at the XML-structure
/// level; NAV's buyer/line cross-field business rules (e.g. an EU buyer for
/// KBAET) are server-side and out of the local validator's structural
/// scope. Using Domestic keeps the emitter's customer path on its
/// well-exercised branch while the test isolates the `<lineVatRate>` shape.
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
            community_vat_number: None,
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("27952890-2-42".to_string()),
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

fn render(kind: VatRateKind, bp: u16) -> String {
    let invoice = invoice_with_kind(kind, bp);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let xml =
        nav_xml::render_invoice_data(&invoice, &series, &domestic_parties(), Currency::Huf, None)
            .expect("emitter must succeed");
    validate_invoice_data(&xml).unwrap_or_else(|e| {
        panic!(
            "validator rejected {kind:?} body: {e}\n--- bytes ---\n{}\n--- end ---",
            String::from_utf8_lossy(&xml)
        )
    });
    String::from_utf8(xml).expect("emit is UTF-8")
}

// ── A/B — the byte-identical `Percent` keystone ──────────────────────────

/// ADR-0101 §5.4 — the regression pin that MUST be impossible to pass if
/// the default path changed. A `Percent`/0% line emits exactly the
/// pre-0101 `<lineVatRate>` block (byte-verbatim, including the emitter's
/// indentation) and NONE of the new choice elements appear anywhere. The
/// refactor keeps the exact Start/Text/End event sequence for `Percent`,
/// so these bytes are identical to the pre-0101 output.
#[test]
fn percent_zero_line_emits_vat_percentage_0_00_byte_identical() {
    let body = render(VatRateKind::Percent, 0);
    // Byte-verbatim block (leaf `<vatPercentage>` inline; container
    // `<lineVatRate>` on its own indented lines — the emitter's shape).
    assert!(
        body.contains(
            "            <lineVatRate>\n              <vatPercentage>0.00</vatPercentage>\n            </lineVatRate>"
        ),
        "Percent/0% must emit the exact pre-0101 lineVatRate block; body:\n{body}"
    );
    // Summary mirror also numeric.
    assert!(
        body.contains(
            "<vatRate>\n              <vatPercentage>0.00</vatPercentage>\n            </vatRate>"
        ),
        "Percent/0% summary must mirror as <vatPercentage>0.00; body:\n{body}"
    );
    for forbidden in [
        "<vatExemption>",
        "<vatOutOfScope>",
        "<vatDomesticReverseCharge>",
        "<case>",
        "<reason>",
    ] {
        assert!(
            !body.contains(forbidden),
            "Percent line must NOT emit {forbidden}; body:\n{body}"
        );
    }
}

/// A `Percent`/27% line is likewise unchanged from pre-0101.
#[test]
fn percent_27_line_emits_vat_percentage_0_27() {
    let body = render(VatRateKind::Percent, 2700);
    assert!(
        body.contains("<vatPercentage>0.27</vatPercentage>"),
        "Percent/27% must emit <vatPercentage>0.27; body:\n{body}"
    );
    // Two occurrences — line lineVatRate + summary vatRate mirror.
    assert_eq!(
        body.matches("<vatPercentage>0.27</vatPercentage>").count(),
        2
    );
}

/// ADR-0101 §5 backward-compat keystone — a pre-0101 side-store
/// `input.json` line (NO `vatRateKind` field) MUST still deserialize, with
/// the kind defaulting to `Percent`. This is what makes storno /
/// modification replays of pre-0101 invoices round-trip byte-identically:
/// the replayed body carries `Percent`, which takes the unchanged emit
/// path. An explicit kind also deserializes (the wire form Session 2 uses).
#[test]
fn pre_0101_line_json_deserializes_as_percent() {
    let pre_0101 =
        r#"{"description":"widget","quantity":"2","unitPrice":1000,"vatRatePercent":27}"#;
    let line: aberp::issue_invoice::LineJson =
        serde_json::from_str(pre_0101).expect("pre-0101 body must still deserialize");
    assert_eq!(
        line.vat_rate_kind,
        VatRateKind::Percent,
        "absent vatRateKind must default to Percent (backward-compat)"
    );

    let explicit = r#"{"description":"w","quantity":"1","unitPrice":1,"vatRatePercent":0,"vatRateKind":"AamExempt"}"#;
    let line2: aberp::issue_invoice::LineJson =
        serde_json::from_str(explicit).expect("explicit vatRateKind must deserialize");
    assert_eq!(line2.vat_rate_kind, VatRateKind::AamExempt);
}

// ── A — the four confirmed kinds ─────────────────────────────────────────

/// AAM (alanyi adómentesség) → `vatExemption` / case `AAM`, on BOTH the
/// line and the summary mirror; no `<vatPercentage>` anywhere.
#[test]
fn aam_exempt_emits_vat_exemption_case_aam_line_and_summary() {
    let body = render(VatRateKind::AamExempt, 0);
    // Leaf children are single-line; the container `<vatExemption>` wraps
    // them across indented lines.
    assert_eq!(
        body.matches("<vatExemption>").count(),
        2,
        "line + summary must both carry vatExemption; body:\n{body}"
    );
    assert_eq!(
        body.matches("<case>AAM</case>").count(),
        2,
        "line + summary must both carry case=AAM; body:\n{body}"
    );
    assert_eq!(
        body.matches("<reason>Alanyi adómentesség [Áfa tv. 187–196. §]</reason>")
            .count(),
        2,
        "the statutory reason must appear on both line and summary; body:\n{body}"
    );
    // Negative: an AAM line does NOT emit vatPercentage.
    assert!(
        !body.contains("<vatPercentage>"),
        "AAM line must NOT emit <vatPercentage>; body:\n{body}"
    );
}

/// Intra-Community exempt supply of GOODS → `vatExemption` / case `KBAET`.
#[test]
fn intra_community_goods_emits_vat_exemption_case_kbaet() {
    let body = render(VatRateKind::IntraCommunityGoods, 0);
    assert_eq!(
        body.matches("<vatExemption>").count(),
        2,
        "intra-Community goods must emit vatExemption on line + summary; body:\n{body}"
    );
    assert_eq!(
        body.matches("<case>KBAET</case>").count(),
        2,
        "case must be KBAET on line + summary; body:\n{body}"
    );
    assert!(!body.contains("<vatPercentage>"));
    assert!(!body.contains("<vatOutOfScope>"));
}

/// Cross-border SERVICE reverse-charged at the customer's member state →
/// `vatOutOfScope` / case `EUFAD37` (NOT an exemption — out of HU scope).
#[test]
fn intra_community_service_reverse_emits_vat_out_of_scope_case_eufad37() {
    let body = render(VatRateKind::IntraCommunityServiceReverse, 0);
    assert_eq!(
        body.matches("<vatOutOfScope>").count(),
        2,
        "cross-border service reverse must emit vatOutOfScope on line + summary; body:\n{body}"
    );
    assert_eq!(
        body.matches("<case>EUFAD37</case>").count(),
        2,
        "case must be EUFAD37 on line + summary; body:\n{body}"
    );
    // Negative — it is out-of-scope, NOT an exemption, and NOT numeric.
    assert!(
        !body.contains("<vatExemption>"),
        "service-reverse must NOT emit vatExemption (that is the goods shape); body:\n{body}"
    );
    assert!(!body.contains("<vatPercentage>"));
}

/// Domestic reverse-charge → the boolean `<vatDomesticReverseCharge>true`,
/// no case code, on both line and summary; no exemption / percentage.
#[test]
fn domestic_reverse_charge_emits_boolean_element() {
    let body = render(VatRateKind::DomesticReverseCharge, 0);
    assert_eq!(
        body.matches("<vatDomesticReverseCharge>true</vatDomesticReverseCharge>")
            .count(),
        2,
        "line + summary must both carry <vatDomesticReverseCharge>true; body:\n{body}"
    );
    assert!(!body.contains("<vatExemption>"));
    assert!(!body.contains("<vatOutOfScope>"));
    assert!(!body.contains("<vatPercentage>"));
    assert!(
        !body.contains("<case>"),
        "the reverse-charge boolean carries no case code; body:\n{body}"
    );
}

// ── F — validator hardening + named-deferred emit loud-fail ───────────────

/// A named-deferred kind (e.g. TAM) is NOT wired for NAV emit yet — the
/// emitter loud-fails (ADR-0101 §3.1 explicit not-yet marker). This is the
/// defence-in-depth behind the preflight shut door: even if a caller
/// bypassed preflight, no deferred-kind body escapes onto the wire.
#[test]
fn named_deferred_kind_loud_fails_emit() {
    let invoice = invoice_with_kind(VatRateKind::TamExempt, 0);
    let series = SeriesCode::new("INV-default".to_string()).unwrap();
    let err =
        nav_xml::render_invoice_data(&invoice, &series, &domestic_parties(), Currency::Huf, None)
            .expect_err("named-deferred kind must loud-fail the emitter");
    let msg = err.to_string();
    assert!(
        msg.contains("named-deferred") || msg.contains("TamExempt"),
        "loud-fail must name the deferral; got: {msg}"
    );
}

/// The validator rejects a `vatExemption` missing its required `<case>`
/// child — the ADR §8.F "should-harden" that was `skip_to_matching_end`ed
/// before ADR-0101. Proves the case/reason modeling is load-bearing.
#[test]
fn validator_rejects_exemption_missing_case() {
    let good = render(VatRateKind::AamExempt, 0);
    // Drop the <case>AAM</case> from BOTH the line and the summary so the
    // body stays well-formed but each vatExemption is missing its case.
    let broken = good.replace("<case>AAM</case>", "");
    let err = validate_invoice_data(broken.as_bytes())
        .expect_err("a vatExemption without <case> must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("case"),
        "rejection must name the missing <case>; got: {msg}"
    );
}
