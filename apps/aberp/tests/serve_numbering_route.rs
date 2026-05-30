//! PR-89 — integration pins for `/api/seller/numbering`.
//!
//! Exercises the typed library helpers
//! ([`aberp::serve::get_seller_numbering_request`] +
//! [`aberp::serve::put_seller_numbering_request`]) against a unique-per-
//! run tenant directory, mirroring the
//! `serve_setup_seller_info_route.rs` posture (no HTTP listener).
//! Pinned invariants:
//!
//! - GET on an absent seller.toml returns the default template
//!   (`INV-default/` + `Counter{pad:5}`, `Never`, start=1).
//! - PUT with a valid template persists the section and a subsequent
//!   GET reflects the saved values.
//! - PUT preserves the identity (`[seller]`, `[seller.address]`) and
//!   bank-account sections of the file (non-destructive merge).
//! - PUT with an illegal-charset Literal loud-fails with
//!   [`NumberingValidationError::Domain(InvalidLiteralCharacter)`]
//!   (NAV invoiceNumber charset gate).
//! - PUT with a multi-counter template loud-fails with
//!   [`NumberingError::MultipleCounters`].

use std::path::PathBuf;

use aberp::numbering::{
    self, NumberingTemplate, ResetPolicy as NumberingResetPolicy, Segment as NumberingSegment,
    YearDigits,
};

fn unique_tmpdir() -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("aberp-pr89-numbering-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("mkdir tmpdir");
    dir
}

/// Absent seller.toml → GET returns the default template. The path-based
/// helper [`numbering::read_numbering_template`] is what the route's
/// library helper [`aberp::serve::get_seller_numbering_request`] flows
/// through; pin the same invariant at this surface so a future
/// refactor that drops the fall-through path surfaces here.
#[test]
fn get_on_absent_file_returns_default_template() {
    let dir = unique_tmpdir();
    let path = dir.join("seller.toml");
    assert!(!path.exists());
    let t = numbering::read_numbering_template(&path).expect("read returns default");
    assert_eq!(t, numbering::default_template());
}

/// PUT a valid template → file exists with the `[seller.numbering]`
/// section + GET reflects it. Round-trip pin.
#[test]
fn put_then_get_round_trips_template() {
    let dir = unique_tmpdir();
    let path = dir.join("seller.toml");
    let template = NumberingTemplate {
        segments: vec![
            NumberingSegment::Literal("ABERP-".to_string()),
            NumberingSegment::Year {
                digits: YearDigits::Four,
            },
            NumberingSegment::Literal("/".to_string()),
            NumberingSegment::Counter { pad_width: 6 },
        ],
        reset_policy: NumberingResetPolicy::OnYearChange,
        start_value: 1247,
    };
    numbering::write_numbering_section(&path, &template).expect("write template");
    let back = numbering::read_numbering_template(&path).expect("read back");
    assert_eq!(back, template);
    // The persisted file actually contains the section header.
    let body = std::fs::read_to_string(&path).expect("read file");
    assert!(body.contains("[seller.numbering]"));
    assert!(body.contains("start_value = 1247"));
}

/// PUT preserves the identity block + bank section above. The non-
/// destructive-write invariant — same posture as PR-72's bank-section
/// merge.
#[test]
fn put_preserves_identity_and_bank_sections() {
    let dir = unique_tmpdir();
    let path = dir.join("seller.toml");
    let preexisting = "\
# ABERP seller config\n\
[seller]\n\
legal_name = \"Áben Consulting KFT.\"\n\
tax_number = \"24904362-2-41\"\n\
\n\
[seller.address]\n\
country_code = \"HU\"\n\
postal_code = \"1037\"\n\
city = \"Budapest\"\n\
street = \"Visszatérő köz 6\"\n\
\n\
[[seller.banks]]\n\
currency = \"HUF\"\n\
account_number = \"12345678-12345678-12345678\"\n\
bank_name = \"Erste Bank\"\n\
swift_bic = \"GIBAHUHB\"\n\
default = true\n";
    std::fs::write(&path, preexisting).expect("seed file");

    let template = NumberingTemplate {
        segments: vec![
            NumberingSegment::Literal("ABERP-".to_string()),
            NumberingSegment::Year {
                digits: YearDigits::Four,
            },
            NumberingSegment::Literal("/".to_string()),
            NumberingSegment::Counter { pad_width: 6 },
        ],
        reset_policy: NumberingResetPolicy::OnYearChange,
        start_value: 1,
    };
    numbering::write_numbering_section(&path, &template).expect("write template");
    let body = std::fs::read_to_string(&path).expect("read after write");
    // Identity preserved.
    assert!(
        body.contains("Áben Consulting KFT."),
        "identity preserved: {body}"
    );
    assert!(
        body.contains("[seller.address]"),
        "address heading preserved: {body}"
    );
    assert!(
        body.contains("city = \"Budapest\""),
        "address city preserved: {body}"
    );
    // Bank block preserved.
    assert!(
        body.contains("[[seller.banks]]"),
        "bank block preserved: {body}"
    );
    assert!(body.contains("Erste Bank"), "bank name preserved: {body}");
    // Numbering section present.
    assert!(
        body.contains("[seller.numbering]"),
        "numbering section present: {body}"
    );
    // Comment preamble preserved.
    assert!(
        body.contains("# ABERP seller config"),
        "comment preamble preserved: {body}"
    );
}

/// Template with an illegal-charset Literal must loud-fail at the
/// validator. The route surfaces this as HTTP 422 with the bilingual
/// operator message; the library helper returns the typed error.
#[test]
fn validator_rejects_backslash_in_literal() {
    let template = NumberingTemplate {
        segments: vec![
            NumberingSegment::Literal("ABERP\\".to_string()),
            NumberingSegment::Counter { pad_width: 4 },
        ],
        reset_policy: NumberingResetPolicy::Never,
        start_value: 1,
    };
    let err = numbering::validate_template(&template).expect_err("backslash must loud-fail");
    match err {
        numbering::NumberingError::InvalidLiteralCharacter {
            segment_index,
            character,
        } => {
            assert_eq!(segment_index, 0);
            assert_eq!(character, '\\');
        }
        other => panic!("expected InvalidLiteralCharacter, got {other:?}"),
    }
}

/// Multiple counters → ambiguous render → loud-fail at validate.
#[test]
fn validator_rejects_multiple_counters() {
    let template = NumberingTemplate {
        segments: vec![
            NumberingSegment::Counter { pad_width: 2 },
            NumberingSegment::Literal("-".to_string()),
            NumberingSegment::Counter { pad_width: 2 },
        ],
        reset_policy: NumberingResetPolicy::Never,
        start_value: 1,
    };
    let err = numbering::validate_template(&template).expect_err("multi-counter must fail");
    assert!(matches!(
        err,
        numbering::NumberingError::MultipleCounters { count: 2 }
    ));
}

/// `format_invoice_number` falls back to the default INV-default/NNNNN
/// shape when the seller.toml is absent — the pre-PR-89 emit invariant
/// the eight migrated emit sites rely on (a legacy tenant without
/// `[seller.numbering]` configured sees zero change).
#[test]
fn format_invoice_number_default_fallback_matches_pre_pr89() {
    let dir = unique_tmpdir();
    let path = dir.join("seller.toml");
    assert!(!path.exists());
    let rendered = numbering::format_invoice_number(&path, 2026, 42);
    // S165 — the emit path carries the build-profile prefix (`TEST-` on
    // dev/test builds, empty on production). Compose from the const so
    // this pins identically under both build flavours.
    assert_eq!(
        rendered,
        format!(
            "{}INV-default/00042",
            aberp::build_profile::INVOICE_NUMBER_TEST_PREFIX
        )
    );
}

/// `format_invoice_number` reads the persisted template and renders
/// against it — the production emit path's invariant. Pinned end-to-end
/// at this surface so the IO + render + template combination stays
/// stable across refactors.
#[test]
fn format_invoice_number_reads_persisted_template() {
    let dir = unique_tmpdir();
    let path = dir.join("seller.toml");
    let template = NumberingTemplate {
        segments: vec![
            NumberingSegment::Literal("ABERP-".to_string()),
            NumberingSegment::Year {
                digits: YearDigits::Four,
            },
            NumberingSegment::Literal("/".to_string()),
            NumberingSegment::Counter { pad_width: 6 },
        ],
        reset_policy: NumberingResetPolicy::OnYearChange,
        start_value: 1,
    };
    numbering::write_numbering_section(&path, &template).expect("write template");
    // S165 — prefix-aware: the emit path prepends `TEST-` on dev/test
    // builds and nothing on production builds.
    let prefix = aberp::build_profile::INVOICE_NUMBER_TEST_PREFIX;
    let rendered = numbering::format_invoice_number(&path, 2026, 1);
    assert_eq!(rendered, format!("{prefix}ABERP-2026/000001"));
    let rendered_later = numbering::format_invoice_number(&path, 2027, 1);
    assert_eq!(rendered_later, format!("{prefix}ABERP-2027/000001"));
}
