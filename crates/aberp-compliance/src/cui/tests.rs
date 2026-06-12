//! Unit tests for the S345 CUI marking vocabulary.

use super::{CuiCategory, CuiMarking, DisseminationControl};

#[test]
fn s345_cui_is_cui_only_for_cui_variant() {
    assert!(CuiMarking::Cui(CuiCategory::Cti).is_cui());
    assert!(!CuiMarking::Unclassified.is_cui());
    assert!(!CuiMarking::Confidential.is_cui());
    assert!(!CuiMarking::Secret.is_cui());
    assert!(!CuiMarking::TopSecret.is_cui());
}

#[test]
fn s345_cui_is_classified_only_for_classification_levels() {
    assert!(CuiMarking::Confidential.is_classified());
    assert!(CuiMarking::Secret.is_classified());
    assert!(CuiMarking::TopSecret.is_classified());
    // Unclassified and CUI are explicitly NOT classified.
    assert!(!CuiMarking::Unclassified.is_classified());
    assert!(!CuiMarking::Cui(CuiCategory::Expt).is_classified());
}

#[test]
fn s345_cui_display_marking_unclassified_and_classified() {
    assert_eq!(CuiMarking::Unclassified.display_marking(), "UNCLASSIFIED");
    assert_eq!(CuiMarking::Confidential.display_marking(), "CONFIDENTIAL");
    assert_eq!(CuiMarking::Secret.display_marking(), "SECRET");
    assert_eq!(CuiMarking::TopSecret.display_marking(), "TOP SECRET");
}

#[test]
fn s345_cui_display_marking_renders_cui_banner_with_abbrev() {
    // CTI and EXPT are CUI Specified — banner carries the SP- prefix (F10).
    assert_eq!(
        CuiMarking::Cui(CuiCategory::Cti).display_marking(),
        "CUI//SP-CTI"
    );
    // PRVCY is CUI Basic — no SP- prefix.
    assert_eq!(
        CuiMarking::Cui(CuiCategory::Prvcy).display_marking(),
        "CUI//PRVCY"
    );
    assert_eq!(
        CuiMarking::Cui(CuiCategory::Expt).display_marking(),
        "CUI//SP-EXPT"
    );
}

#[test]
fn s367_cui_specified_categories_carry_sp_prefix() {
    // F10: CUI Specified categories take the SP- banner prefix; CUI Basic do not.
    assert!(CuiCategory::Cti.is_specified());
    assert!(CuiCategory::Expt.is_specified());
    // Conservative subset — the rest are treated as CUI Basic.
    for basic in [
        CuiCategory::Prvcy,
        CuiCategory::Crit,
        CuiCategory::Lei,
        CuiCategory::Ifg,
        CuiCategory::Inf,
        CuiCategory::Isvi,
        CuiCategory::Proc,
        CuiCategory::Prop,
    ] {
        assert!(!basic.is_specified(), "{basic:?} should be CUI Basic");
        // A Basic category's banner must NOT carry SP-.
        assert!(
            !CuiMarking::Cui(basic).display_marking().contains("SP-"),
            "{basic:?} banner must not carry SP-"
        );
    }
}

#[test]
fn s345_cui_category_abbreviations_are_all_distinct() {
    let cats = [
        CuiCategory::Cti,
        CuiCategory::Prvcy,
        CuiCategory::Expt,
        CuiCategory::Crit,
        CuiCategory::Lei,
        CuiCategory::Ifg,
        CuiCategory::Inf,
        CuiCategory::Isvi,
        CuiCategory::Proc,
        CuiCategory::Prop,
    ];
    let mut seen = std::collections::HashSet::new();
    for c in cats {
        assert!(
            seen.insert(c.abbreviation()),
            "duplicate abbreviation {:?}",
            c.abbreviation()
        );
    }
    assert_eq!(seen.len(), 10, "expected 10 distinct starter categories");
}

#[test]
fn s345_cui_category_roundtrip() {
    let c = CuiCategory::Isvi;
    let json = serde_json::to_string(&c).expect("serialize");
    let back: CuiCategory = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(c, back);
}

#[test]
fn s345_cui_marking_roundtrip() {
    for m in [
        CuiMarking::Unclassified,
        CuiMarking::Cui(CuiCategory::Proc),
        CuiMarking::Confidential,
        CuiMarking::Secret,
        CuiMarking::TopSecret,
    ] {
        let json = serde_json::to_string(&m).expect("serialize");
        let back: CuiMarking = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }
}

// ── S360 (ADR-0077) — DisseminationControl + to_banner_str ──────────────────

#[test]
fn s360_dissemination_control_abbreviations_are_all_distinct() {
    let controls = [
        DisseminationControl::NoForn,
        DisseminationControl::FedCon,
        DisseminationControl::NoCon,
        DisseminationControl::DlOnly,
    ];
    let mut seen = std::collections::HashSet::new();
    for c in controls {
        assert!(
            seen.insert(c.abbreviation()),
            "duplicate abbreviation {:?}",
            c.abbreviation()
        );
    }
    assert_eq!(seen.len(), 4, "expected 4 distinct starter controls");
}

#[test]
fn s360_dissemination_control_roundtrip() {
    let c = DisseminationControl::NoForn;
    let json = serde_json::to_string(&c).expect("serialize");
    let back: DisseminationControl = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(c, back);
}

#[test]
fn s360_to_banner_str_with_no_dissemination_equals_display_marking() {
    // With no controls the banner is exactly display_marking — the honest
    // "category banner, no further limits" form.
    for m in [
        CuiMarking::Unclassified,
        CuiMarking::Cui(CuiCategory::Cti),
        CuiMarking::Secret,
    ] {
        assert_eq!(m.to_banner_str(&[]), m.display_marking());
    }
}

#[test]
fn s360_to_banner_str_appends_single_dissemination_segment() {
    assert_eq!(
        CuiMarking::Cui(CuiCategory::Cti).to_banner_str(&[DisseminationControl::NoForn]),
        "CUI//SP-CTI//NOFORN"
    );
    assert_eq!(
        CuiMarking::Secret.to_banner_str(&[DisseminationControl::NoForn]),
        "SECRET//NOFORN"
    );
}

#[test]
fn s360_to_banner_str_joins_multiple_controls_with_slash() {
    assert_eq!(
        CuiMarking::Cui(CuiCategory::Prvcy)
            .to_banner_str(&[DisseminationControl::FedCon, DisseminationControl::NoForn]),
        "CUI//PRVCY//FEDCON/NOFORN"
    );
}
