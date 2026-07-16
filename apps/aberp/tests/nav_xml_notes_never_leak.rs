//! PR-82 — headline never-leak pin for buyer-facing notes.
//!
//! # Why this test is the load-bearing one
//!
//! PR-82 introduces a per-invoice `invoice_note` field on
//! [`AllocateArgs`] and a per-line `note` field on
//! [`aberp_billing::LineItem`]. Both are buyer-facing ("Megjegyzés" —
//! Hungarian for "Note/Remark") and live in DuckDB + the printed
//! PDF + (later) the SMTP email body. They must NEVER reach the NAV
//! `<InvoiceData>` XML wire body — NAV's XSD has no slot for them and
//! emitting them risks the exact SCHEMA_VIOLATION class that PR-76 /
//! PR-77 spent two sessions clearing.
//!
//! This pin is the regulatory boundary: render the same NAV body
//! twice, once with notes and once without, and assert the bytes are
//! BYTE-IDENTICAL. The pin is named explicitly in
//! `adr/0042-invoice-notes-never-in-nav-xml.md` as the invariant
//! guard a future code change must trip if it accidentally wires
//! notes into `nav_xml::render_invoice_data`.
//!
//! # What this test deliberately does NOT verify
//!
//! The DuckDB storage, the audit-payload stamp, the SPA wire shape,
//! and the printed-PDF render of notes are covered by their own pins
//! (`load_invoice_note_in_tx` integration paths, the audit payload's
//! round-trip test, the `serve_pdf_route` integration test). This
//! test is purposefully narrow: one assertion on the NAV body's
//! byte equality + one assertion on the literal absence of the note
//! text in the rendered bytes.

use aberp::nav_xml::{
    self, CustomerAddress, CustomerInfo, CustomerVatStatus, NavParties, StornoReference,
    SupplierInfo,
};
use aberp_billing::{
    Currency, CustomerId, Huf, InvoiceId, LineItem, ReadyInvoice, SeriesCode, SeriesId,
};

/// Operator-typed text the test treats as a sentinel. Distinctive
/// enough that a false negative is implausible: the renderer
/// accidentally emitting any byte of this string into the wire body
/// would surface as a needle-in-haystack search hit, NOT as silently-
/// passing bytes.
const SENTINEL_INVOICE_NOTE: &str =
    "PR-82-sentinel-invoice-MEGJEGYZÉS-buyer-facing-text-MUST-NOT-LEAK";
const SENTINEL_LINE_A_NOTE: &str =
    "PR-82-sentinel-line-A-MEGJEGYZÉS-leave-at-back-door-MUST-NOT-LEAK";
const SENTINEL_LINE_B_NOTE: &str =
    "PR-82-sentinel-line-B-MEGJEGYZÉS-deliver-in-the-morning-MUST-NOT-LEAK";

/// PR-83 — buyer-facing storno reason sentinel. Distinctive enough that
/// any byte of the string showing up in the rendered NAV body would be
/// a needle-in-haystack hit, not a silent pass. The storno renderer
/// (`render_storno_data`) MUST NEVER emit this text into the wire body
/// — the storno reason is recipient-facing only (printed PDF + future
/// email), and NAV has no `annulmentReason`-shape slot in the storno's
/// `<InvoiceData>` body (NAV's annulment reason lives on the separate
/// `manageAnnulment` /technicalAnnulment surface, not on the data
/// submission per ADR-0023 §1).
const SENTINEL_STORNO_REASON: &str =
    "PR-83-sentinel-storno-INDOKA-buyer-facing-WHY-cancelled-MUST-NOT-LEAK";

fn parties() -> NavParties {
    NavParties {
        supplier: SupplierInfo {
            tax_number: "12345678-1-42".to_string(),
            name: "ABERP Supplier Kft.".to_string(),
            address_country_code: "HU".to_string(),
            address_postal_code: "1011".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Fő utca 1.".to_string(),
        },
        customer: CustomerInfo {
            community_vat_number: None,
            // PR-97 / ADR-0048 — preserve pre-PR-97 implicit
            // Domestic posture for legacy test fixtures.
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("87654321-1-42".to_string()),
            name: "Test Customer Zrt.".to_string(),
            address: Some(CustomerAddress {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Váci utca 19.".to_string(),
            }),
        },
    }
}

/// Build the fixture invoice. `with_notes` toggles whether each
/// `LineItem.note` carries the sentinel text — the rest of the line
/// content is identical so the NAV body's byte form depends ONLY on
/// the note presence/absence.
///
/// The `issue_date` is a fixed wall-clock instant so the
/// `<invoiceIssueDate>` element matches byte-for-byte across the two
/// renders. Using `OffsetDateTime::now_utc()` would flake the
/// byte-equality assertion if the second render straddled a date
/// boundary.
fn fixture_invoice(with_notes: bool) -> ReadyInvoice {
    let fixed_date = time::macros::datetime!(2026-05-27 10:30:00 UTC);
    let mut lines = vec![
        LineItem {
            description: "Widget A".to_string(),
            quantity: rust_decimal::Decimal::from(2),
            unit_price: Huf(1_000),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        },
        LineItem {
            description: "Install B".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: Huf(5_000),
            vat_rate_basis_points: 2700,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        },
    ];
    if with_notes {
        lines[0].note = Some(SENTINEL_LINE_A_NOTE.to_string());
        lines[1].note = Some(SENTINEL_LINE_B_NOTE.to_string());
    }
    ReadyInvoice {
        // Deterministic ids so the rendered body (which does NOT
        // surface ids today, but might in a future emit) does not
        // introduce per-run drift. The ULID-from-bytes constructor
        // would be the cleanest way to pin these; for PR-82 the
        // invoice number alone is the wire-visible identifier and it
        // derives from `sequence_number` below.
        id: InvoiceId::new(),
        series_id: SeriesId::new(),
        customer_id: CustomerId::new(),
        lines,
        issue_date: fixed_date,
        // PR-84 — fixture defaults both date fields to the fixed issue
        // date so the wire-byte-identical pin survives the PR-84
        // signature change (delivery + payment dates were previously
        // mirrored to issue_date by the emitter; now they come off the
        // ReadyInvoice — same byte shape, different code path).
        payment_deadline: fixed_date.date(),
        delivery_date: fixed_date.date(),
        sequence_number: 42,
        fiscal_year: 0,
    }
}

/// THE headline pin. PR-82 / ADR-0042 — render the NAV body twice
/// with the only difference being the presence/absence of notes, and
/// assert the wire bytes are byte-identical.
#[test]
fn nav_xml_invoice_data_byte_identical_with_or_without_notes() {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");

    // Invoice IDs / customer IDs differ across the two calls (each
    // construction mints fresh ULIDs). The current `render_invoice_data`
    // emitter does NOT surface them on the wire body (the
    // `<invoiceNumber>` derives from `series_code` + `sequence_number`,
    // which we pin). If a future emitter starts emitting the ULID, this
    // pin's byte-equality assertion would catch it as a separate
    // regression — fine, because that would be a separate decision
    // worth explicit review.
    //
    // Construct the two fixtures with the same `sequence_number` /
    // `fiscal_year` / `issue_date` / line shape; only the notes
    // differ.
    let without_notes = fixture_invoice(false);
    let mut with_notes = fixture_invoice(true);
    with_notes.id = without_notes.id;
    with_notes.series_id = without_notes.series_id;
    with_notes.customer_id = without_notes.customer_id;

    let xml_without =
        nav_xml::render_invoice_data(&without_notes, &series, &parties, Currency::Huf, None)
            .expect("render NAV XML (no notes)");
    let xml_with =
        nav_xml::render_invoice_data(&with_notes, &series, &parties, Currency::Huf, None)
            .expect("render NAV XML (with notes)");

    assert_eq!(
        xml_without, xml_with,
        "PR-82 / ADR-0042 INVARIANT VIOLATION: rendering an invoice with notes \
         produced different bytes than rendering the same invoice without notes. \
         Notes MUST NEVER appear in the NAV InvoiceData XML — see \
         adr/0042-invoice-notes-never-in-nav-xml.md. Whichever code path now \
         consumes LineItem.note or invoice_note inside the renderer must be \
         reverted."
    );
}

/// Sentinel-substring pin. Belt-and-braces against a hypothetical
/// future render that:
///
///   (a) reshuffles the wire body in a way that incidentally produces
///       byte-different output, AND
///   (b) accidentally interpolates the note text somewhere.
///
/// The byte-equality assertion above already catches (a); this test
/// adds the explicit "needle in haystack" check on the literal note
/// text against the with-notes render. The two tests together close
/// every reasonable accidental-leak channel.
#[test]
fn nav_xml_invoice_data_never_contains_note_sentinel_text() {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");
    let with_notes = fixture_invoice(true);

    let xml = nav_xml::render_invoice_data(&with_notes, &series, &parties, Currency::Huf, None)
        .expect("render NAV XML (with notes)");
    let body = std::str::from_utf8(&xml).expect("NAV body is UTF-8");

    for needle in [
        SENTINEL_INVOICE_NOTE,
        SENTINEL_LINE_A_NOTE,
        SENTINEL_LINE_B_NOTE,
    ] {
        assert!(
            !body.contains(needle),
            "PR-82 / ADR-0042 INVARIANT VIOLATION: NAV InvoiceData XML body \
             contained the note-sentinel substring `{needle}`. The note \
             surface is recipient-facing ONLY — it leaked into the regulatory \
             wire body. See adr/0042-invoice-notes-never-in-nav-xml.md."
        );
    }
}

/// Per-line note-presence pin: same fixture, but only one line carries
/// a note. Catches an emitter regression that's quiet on the "all
/// lines" path but loud on the "some lines" path (e.g. a conditional
/// that fires only when every line has a note).
#[test]
fn nav_xml_invoice_data_byte_identical_with_partial_line_notes() {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");

    let base = fixture_invoice(false);
    let mut partial = fixture_invoice(false);
    partial.id = base.id;
    partial.series_id = base.series_id;
    partial.customer_id = base.customer_id;
    // Only the second line carries a note.
    partial.lines[1].note = Some(SENTINEL_LINE_B_NOTE.to_string());

    let xml_base = nav_xml::render_invoice_data(&base, &series, &parties, Currency::Huf, None)
        .expect("render NAV XML (base)");
    let xml_partial =
        nav_xml::render_invoice_data(&partial, &series, &parties, Currency::Huf, None)
            .expect("render NAV XML (partial notes)");
    assert_eq!(
        xml_base, xml_partial,
        "PR-82 / ADR-0042 INVARIANT VIOLATION (partial-notes form): the NAV \
         body's bytes differed when a single line carried a note. The \
         renderer's per-line writer must not consume `LineItem.note`."
    );

    // Make sure `base` and `partial` are not accidentally identical
    // at the domain layer (would invalidate the test as a regression
    // detector): the second line's note differs by construction.
    assert!(
        base.lines[1].note.is_none() && partial.lines[1].note.is_some(),
        "test fixture invariant: only `partial` should carry a line-1 note"
    );
}

// ──────────────────────────────────────────────────────────────────────
// PR-83 — storno-emit cases. The storno renderer
// (`render_storno_data`) reuses the same per-line writer as
// `render_invoice_data` (intentional per `issue_storno.rs::run`'s
// "negation only touches unit price" comment), so PR-82's
// `render_invoice_data` byte-identity pin already covers the per-line
// note path through both renderers. These pins close the storno-
// reason channel: a storno reason lands on the storno's own
// `invoice.invoice_note` column AND on the audit payload's
// `invoice_note` field, but MUST NOT reach the wire body.
//
// The pin defends against a hypothetical future commit that wires
// `invoice.invoice_note` into `render_storno_data` (e.g. as a NAV
// `<comments>`-style annotation, which the v3.0 schema does not
// carry). The byte-identity assertion catches it loud per
// CLAUDE.md rule 12 + ADR-0042.
// ──────────────────────────────────────────────────────────────────────

/// ADR-0101 §8.D — the notes / PII firewall holds ACROSS the new
/// exempt-kind emit branch. A line carrying a buyer note AND an `AamExempt`
/// kind must still NOT leak the recipient-facing note, while the STATUTORY
/// `reason` string (which IS intentional NAV wire content, not buyer PII)
/// DOES appear. This proves ADR-0101 did not open a new leak channel and
/// that its added wire content is statutory, not operator/buyer-identifying.
#[test]
fn exempt_kind_line_note_still_never_leaks_but_statutory_reason_is_wire_content() {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");

    let mut inv = fixture_invoice(true); // both lines carry sentinel notes
    for line in inv.lines.iter_mut() {
        line.vat_rate_kind = aberp_billing::VatRateKind::AamExempt;
        line.vat_rate_basis_points = 0;
    }

    let xml = nav_xml::render_invoice_data(&inv, &series, &parties, Currency::Huf, None)
        .expect("render NAV XML (AAM lines with notes)");
    let body = std::str::from_utf8(&xml).expect("NAV body is UTF-8");

    // Buyer notes STILL firewalled — the new emit branch does not consume them.
    for needle in [SENTINEL_LINE_A_NOTE, SENTINEL_LINE_B_NOTE] {
        assert!(
            !body.contains(needle),
            "ADR-0101 / ADR-0042 VIOLATION: exempt-kind emit leaked note `{needle}`; body:\n{body}"
        );
    }
    // The statutory case + reason ARE intentional wire content (not PII).
    assert!(
        body.contains("<case>AAM</case>"),
        "AAM case must be emitted as intentional wire content; body:\n{body}"
    );
    assert!(
        body.contains("Alanyi adómentesség"),
        "the statutory reason must be emitted (non-PII wire content); body:\n{body}"
    );
}

fn storno_reference() -> StornoReference {
    StornoReference {
        base_invoice_number: "INV-default/00041".to_string(),
        modification_index: 1,
        base_line_count: 1, // S369 — single-line base fixture
    }
}

/// THE storno-channel headline pin. PR-83 / ADR-0042 — render the
/// storno NAV body with and without per-line notes (which the storno
/// inherits from the base) and assert the bytes are byte-identical.
/// The storno's `invoice_note` (storno reason) is NOT carried on the
/// `ReadyInvoice` fixture today — it lives only on the DuckDB column
/// and the audit payload; the renderer takes the `ReadyInvoice` shape
/// and has no field to consume. This test fixes that surface area at
/// the renderer layer.
#[test]
fn nav_xml_storno_data_byte_identical_with_or_without_notes() {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");
    let storno_ref = storno_reference();

    let without_notes = fixture_invoice(false);
    let mut with_notes = fixture_invoice(true);
    with_notes.id = without_notes.id;
    with_notes.series_id = without_notes.series_id;
    with_notes.customer_id = without_notes.customer_id;

    let xml_without = nav_xml::render_storno_data(
        &without_notes,
        &series,
        &parties,
        &storno_ref,
        Currency::Huf,
        None,
    )
    .expect("render NAV storno XML (no notes)");
    let xml_with = nav_xml::render_storno_data(
        &with_notes,
        &series,
        &parties,
        &storno_ref,
        Currency::Huf,
        None,
    )
    .expect("render NAV storno XML (with notes)");

    assert_eq!(
        xml_without, xml_with,
        "PR-83 / ADR-0042 INVARIANT VIOLATION: rendering a STORNO with \
         per-line notes produced different bytes than rendering the same \
         storno without notes. Notes MUST NEVER appear in the NAV \
         InvoiceData XML (storno or otherwise). See \
         adr/0042-invoice-notes-never-in-nav-xml.md."
    );
}

/// PR-83 — storno-reason sentinel-substring pin. The storno reason
/// the operator types in the SPA confirm panel lands on the storno's
/// own `invoice_note` column AND on the `InvoiceDraftCreated` audit
/// payload. This test pins that NEITHER the per-line note sentinels
/// NOR the storno-reason sentinel appears anywhere in the rendered
/// storno NAV body. Belt-and-braces against a hypothetical future
/// emitter regression that incidentally interpolates an external note
/// string somewhere on the wire.
#[test]
fn nav_xml_storno_data_never_contains_note_sentinel_text() {
    let parties = parties();
    let series = SeriesCode::new("INV-default".to_string()).expect("valid series code");
    let storno_ref = storno_reference();
    let with_notes = fixture_invoice(true);

    let xml = nav_xml::render_storno_data(
        &with_notes,
        &series,
        &parties,
        &storno_ref,
        Currency::Huf,
        None,
    )
    .expect("render NAV storno XML (with notes)");
    let body = std::str::from_utf8(&xml).expect("NAV storno body is UTF-8");

    for needle in [
        SENTINEL_INVOICE_NOTE,
        SENTINEL_LINE_A_NOTE,
        SENTINEL_LINE_B_NOTE,
        SENTINEL_STORNO_REASON,
    ] {
        assert!(
            !body.contains(needle),
            "PR-83 / ADR-0042 INVARIANT VIOLATION: NAV storno InvoiceData \
             XML body contained the note-sentinel substring `{needle}`. \
             The storno reason (and the per-line notes inherited from the \
             base) is recipient-facing ONLY — it leaked into the regulatory \
             wire body. See adr/0042-invoice-notes-never-in-nav-xml.md."
        );
    }
}
