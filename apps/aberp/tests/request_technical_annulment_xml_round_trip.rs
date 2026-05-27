//! Annulment emitter pair-up — minimum-children + structural shape
//! (PR-12, ADR-0025).
//!
//! Parallels `issue_storno_xml_round_trip.rs` and
//! `issue_modification_xml_round_trip.rs` for the technical-annulment
//! path. The TWO sources of truth for "what NAV v3.0
//! `<InvoiceAnnulment>` looks like in ABERP" today are:
//!
//!   1. `apps/aberp/src/nav_xml.rs::render_annulment_data` — the
//!      emitter.
//!   2. `apps/aberp/src/request_technical_annulment.rs::check_annulment_xml_minimum`
//!      — the call-site sanity check (ADR-0025 §4) that runs against
//!      every rendered body before disk-write.
//!
//! The full `validate_annulment_data` runtime validator is **DEFERRED**
//! per ADR-0025 §4: the trigger is the future `submit-annulment` PR
//! that lands the NAV `manageAnnulment` wire call. Until then, this
//! file's tests are the load-bearing structural pin on the emitter's
//! output.
//!
//! This file lives under `apps/aberp/tests/` (integration tests) so
//! it exercises the public `nav_xml::render_annulment_data` surface
//! the same way the storno/modify integration tests exercise their
//! emitters — cross-crate consumers (`request_technical_annulment.rs`,
//! the future `submit_annulment.rs`) see the same shape.

use aberp::nav_xml::{self, AnnulmentReference};

const BASE_INVOICE_NUMBER: &str = "INV-default/00007";

fn build_minimal_reference() -> AnnulmentReference {
    AnnulmentReference {
        base_invoice_number: BASE_INVOICE_NUMBER.to_string(),
        annulment_code: "ERRATIC_DATA",
        reason: "test invoice accidentally sent to production".to_string(),
    }
}

/// **Structural happy-path pin.** The emitter MUST produce bytes
/// that:
///
///   1. Open with the XML declaration.
///   2. Carry the `<InvoiceAnnulment>` root with the
///      `OSA/3.0/annul` namespace (ADR-0025 §"Surfaced conflict 1").
///   3. Contain all four required children in document order:
///      `<annulmentReference>`, `<annulmentTimestamp>`,
///      `<annulmentCode>`, `<annulmentReason>` (ADR-0025 §4).
///   4. Carry the operator-supplied annulment code as the canonical
///      SCREAMING_SNAKE_CASE wire form (NOT the clap-hyphen form).
///   5. Carry the base invoice number verbatim.
///
/// CLAUDE.md rule 9: each assertion targets a load-bearing structural
/// invariant — a future refactor that changes the namespace, drops
/// the timestamp, or lowercases the code would fail one specific
/// assertion (loud).
#[test]
fn render_annulment_data_produces_well_formed_body() {
    let r = build_minimal_reference();
    let xml = nav_xml::render_annulment_data(&r).expect("emitter must succeed");
    let s = std::str::from_utf8(&xml).expect("emitter output must be UTF-8");

    // 1. XML decl + 2. root element + namespace.
    assert!(
        s.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
        "missing XML declaration"
    );
    assert!(
        s.contains("<InvoiceAnnulment"),
        "missing <InvoiceAnnulment> root: {s}"
    );
    assert!(
        s.contains("xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/annul\""),
        "missing annul namespace per ADR-0025 §1: {s}"
    );

    // 3. Document-order pin: each child appears, and they appear in
    //    the order ADR-0025 §4 names.
    let pos_ref = s.find("<annulmentReference>").expect("reference present");
    let pos_ts = s.find("<annulmentTimestamp>").expect("timestamp present");
    let pos_code = s.find("<annulmentCode>").expect("code present");
    let pos_reason = s.find("<annulmentReason>").expect("reason present");
    assert!(
        pos_ref < pos_ts && pos_ts < pos_code && pos_code < pos_reason,
        "ADR-0025 §4 document order violated: ref={pos_ref} ts={pos_ts} code={pos_code} reason={pos_reason}"
    );

    // 4. Code wire form.
    assert!(
        s.contains(">ERRATIC_DATA<"),
        "annulment code must be canonical SCREAMING_SNAKE_CASE wire form: {s}"
    );
    // Defence-in-depth: the clap-hyphen form must NOT leak through.
    assert!(
        !s.contains("erratic-data"),
        "clap-hyphen form leaked into the wire body: {s}"
    );

    // 5. Base invoice number verbatim.
    assert!(
        s.contains(&format!(">{}<", BASE_INVOICE_NUMBER)),
        "base invoice number must appear verbatim in <annulmentReference>: {s}"
    );
}

/// **Hostile-reason F9 pin.** Operator-supplied reason text may
/// carry JSON-hostile characters (quotes, backslashes, control
/// chars, non-ASCII). The XML emitter must escape them per
/// `quick_xml`'s text-element discipline — same posture as
/// `audit_payloads.rs::technical_annulment_round_trips_with_hostile_reason`
/// for the audit payload. This integration test pins the symmetric
/// posture on the XML emitter.
#[test]
fn render_annulment_data_escapes_hostile_reason() {
    let r = AnnulmentReference {
        base_invoice_number: BASE_INVOICE_NUMBER.to_string(),
        annulment_code: "ERRATIC_INVOICE_NUMBER",
        reason: "ünïcödé and \"quotes\" & ampersands <gt> here".to_string(),
    };
    let xml = nav_xml::render_annulment_data(&r).expect("emitter must succeed");
    let s = std::str::from_utf8(&xml).expect("UTF-8");

    // The hostile characters must be XML-escaped — the raw `&` and
    // `<` MUST NOT survive into the body's text content, but the
    // ünïcödé MAY (UTF-8 is the document encoding so non-ASCII is
    // legal verbatim).
    let body_start = s.find("<annulmentReason>").unwrap();
    let body_end = s.find("</annulmentReason>").unwrap();
    let body = &s[body_start..body_end];
    assert!(
        body.contains("&amp;"),
        "raw ampersand must be escaped to &amp;: {body}"
    );
    assert!(
        body.contains("&lt;gt&gt;") || body.contains("&lt;gt>"),
        "raw < must be escaped: {body}"
    );
    // The quote handling depends on quick_xml's policy — accept either
    // escaped (`&quot;`) or verbatim, since both are legal inside
    // element-content text per XML spec. The assertion just guards
    // against silent corruption.
    assert!(
        body.contains("\"quotes\"") || body.contains("&quot;quotes&quot;"),
        "operator quotes must round-trip in some legal form: {body}"
    );
    assert!(
        body.contains("ünïcödé"),
        "non-ASCII characters must survive verbatim under UTF-8: {body}"
    );
}

/// **Timestamp shape pin.** ADR-0025 §4 commits to the ISO 8601 UTC
/// shape `YYYY-MM-DDTHH:MM:SSZ`. If a future emitter regression
/// drops the `Z` suffix or flips the format to NAV's compressed
/// `YYYYMMDDhhmmss` (which is what the SOAP `requestTimestamp` uses,
/// for context), this test fires.
#[test]
fn render_annulment_data_timestamp_is_iso8601_utc_z() {
    let r = build_minimal_reference();
    let xml = nav_xml::render_annulment_data(&r).expect("emitter must succeed");
    let s = std::str::from_utf8(&xml).expect("UTF-8");
    let ts_start = s.find("<annulmentTimestamp>").unwrap() + "<annulmentTimestamp>".len();
    let ts_end = s.find("</annulmentTimestamp>").unwrap();
    let ts = &s[ts_start..ts_end];
    // Shape: 4 digits, '-', 2 digits, '-', 2 digits, 'T', 2 digits,
    // ':', 2 digits, ':', 2 digits, 'Z'. Total 20 chars.
    assert_eq!(
        ts.len(),
        20,
        "timestamp must be `YYYY-MM-DDTHH:MM:SSZ` (20 chars): got {ts:?}"
    );
    assert!(
        ts.ends_with('Z'),
        "timestamp must end with Z per ADR-0025 §4: got {ts:?}"
    );
    assert!(
        ts.chars().nth(4) == Some('-')
            && ts.chars().nth(7) == Some('-')
            && ts.chars().nth(10) == Some('T')
            && ts.chars().nth(13) == Some(':')
            && ts.chars().nth(16) == Some(':'),
        "timestamp structure must match `YYYY-MM-DDTHH:MM:SSZ`: got {ts:?}"
    );
}
