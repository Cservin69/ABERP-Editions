//! Unit tests for the S345 AVL / DPAS types.

use super::{
    ApprovedSupplierEntry, DpasPriority, DpasRating, ExportScreeningStatus, PartnerRef,
    ProgramSymbolError, QualLevel,
};

#[test]
fn s367_unrated_supplier_is_dpas_none() {
    // F13: "unrated commercial order" is the absence of a rating, not a variant.
    let entry = ApprovedSupplierEntry {
        partner_id: PartnerRef("p".to_string()),
        qualification_level: QualLevel::Bid,
        dpas: None,
        screening: ExportScreeningStatus::default(),
        last_audit_at_ms: None,
    };
    assert!(entry.dpas.is_none());
}

#[test]
fn s345_export_screening_status_defaults_to_not_screened() {
    assert_eq!(
        ExportScreeningStatus::default(),
        ExportScreeningStatus::NotScreened
    );
}

#[test]
fn s345_qual_level_bid_and_deliver_gating() {
    // Bid: may bid, may NOT deliver.
    assert!(QualLevel::Bid.can_bid());
    assert!(!QualLevel::Bid.can_deliver());
    // Approved: may bid AND deliver.
    assert!(QualLevel::Approved.can_bid());
    assert!(QualLevel::Approved.can_deliver());
    // Disapproved: neither.
    assert!(!QualLevel::Disapproved.can_bid());
    assert!(!QualLevel::Disapproved.can_deliver());
}

#[test]
fn s345_avl_entry_construction_and_roundtrip() {
    let entry = ApprovedSupplierEntry {
        partner_id: PartnerRef("partner-4711".to_string()),
        qualification_level: QualLevel::Approved,
        dpas: Some(DpasRating::new(DpasPriority::Dx, "A1").expect("valid rating")),
        screening: ExportScreeningStatus::Clear,
        last_audit_at_ms: Some(1_700_000_000_000),
    };
    let json = serde_json::to_string(&entry).expect("serialize");
    let back: ApprovedSupplierEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(entry, back);
    assert_eq!(back.partner_id, PartnerRef("partner-4711".to_string()));
    assert!(back.qualification_level.can_deliver());
}

#[test]
fn s345_avl_entry_unaudited_defaults() {
    // A freshly-listed supplier: bid-only, unrated, unscreened, never audited.
    let entry = ApprovedSupplierEntry {
        partner_id: PartnerRef("partner-new".to_string()),
        qualification_level: QualLevel::Bid,
        dpas: None,
        screening: ExportScreeningStatus::default(),
        last_audit_at_ms: None,
    };
    assert!(entry.qualification_level.can_bid());
    assert!(!entry.qualification_level.can_deliver());
    assert!(entry.dpas.is_none());
    assert_eq!(entry.screening, ExportScreeningStatus::NotScreened);
    assert!(entry.last_audit_at_ms.is_none());
}

#[test]
fn s345_dpas_rating_roundtrip() {
    for r in [
        DpasRating::new(DpasPriority::Do, "A1").unwrap(),
        DpasRating::new(DpasPriority::Dx, "A7").unwrap(),
        DpasRating::new(DpasPriority::Do, "C1").unwrap(),
    ] {
        let json = serde_json::to_string(&r).expect("serialize");
        let back: DpasRating = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
    }
}

// ── S361 / PR-48 (ADR-0078) — storage-string newtype validation ──
//
// The partners `dpas_rating` / `export_screening_status` columns and the
// `supplier.*` audit payloads store the canonical `as_str` form; the firing
// site (later session) validates an inbound string through `parse` /
// `from_storage_str` before it reaches the column / ledger. These pin that the
// `as_str` ⇄ `parse` pair round-trips real ratings and rejects garbage, so a
// malformed value can never reach storage.

#[test]
fn s361_dpas_rating_storage_str_round_trips_every_variant() {
    for r in [
        DpasRating::new(DpasPriority::Do, "A1").unwrap(),
        DpasRating::new(DpasPriority::Dx, "A7").unwrap(),
        DpasRating::new(DpasPriority::Do, "C1").unwrap(),
        DpasRating::new(DpasPriority::Dx, "F1").unwrap(),
    ] {
        let s = r.as_str();
        assert_eq!(
            DpasRating::parse(&s).expect("round-trip"),
            r,
            "round-trip mismatch for {s}"
        );
    }
}

#[test]
fn s361_dpas_rating_storage_tokens_are_pinned_and_reject_unknown() {
    // 15 CFR 700.12 form: <DO|DX>-<program symbol> (e.g. the regulation's own
    // worked example DO-A1).
    assert_eq!(
        DpasRating::new(DpasPriority::Do, "A1").unwrap().as_str(),
        "DO-A1"
    );
    assert_eq!(
        DpasRating::new(DpasPriority::Dx, "A7").unwrap().as_str(),
        "DX-A7"
    );
    // Unknown / wrong-case / out-of-range strings fail loud — never a silent
    // default.
    assert!(DpasRating::parse("").is_err());
    assert!(DpasRating::parse("do-a1").is_err()); // lowercase priority
    assert!(DpasRating::parse("DX").is_err()); // no program symbol
    assert!(DpasRating::parse("DO-G1").is_err()); // letter out of A-F
    assert!(DpasRating::parse("DO-A0").is_err()); // digit out of 1-9
    assert!(DpasRating::parse("DO-A12").is_err()); // too long
    assert!(matches!(
        DpasRating::parse("DZ-A1"),
        Err(ProgramSymbolError::BadPriority(_))
    ));
    assert!(matches!(DpasRating::validate_program_symbol("A1"), Ok(())));
    assert!(matches!(
        DpasRating::validate_program_symbol("ZZ"),
        Err(ProgramSymbolError::BadSymbol(_))
    ));
}

#[test]
fn s361_export_screening_status_storage_str_round_trips_every_variant() {
    for st in [
        ExportScreeningStatus::NotScreened,
        ExportScreeningStatus::Clear,
        ExportScreeningStatus::Hit,
        ExportScreeningStatus::Inconclusive,
    ] {
        let s = st.as_str();
        assert_eq!(
            ExportScreeningStatus::from_storage_str(s).expect("round-trip"),
            st,
            "round-trip mismatch for {s}"
        );
    }
}

#[test]
fn s361_export_screening_status_tokens_match_brief_vocab_and_reject_unknown() {
    // The exact `clear` / `hit` / `inconclusive` / `not_screened` vocab the
    // brief pins for the column + the `supplier.export_screened` payload.
    assert_eq!(ExportScreeningStatus::NotScreened.as_str(), "not_screened");
    assert_eq!(ExportScreeningStatus::Clear.as_str(), "clear");
    assert_eq!(ExportScreeningStatus::Hit.as_str(), "hit");
    assert_eq!(ExportScreeningStatus::Inconclusive.as_str(), "inconclusive");
    // A mis-parse to Clear would mark an unscreened / hit supplier clear to
    // transact — the worst-class export-control bug. Must fail loud.
    assert!(ExportScreeningStatus::from_storage_str("CLEAR").is_err());
    assert!(ExportScreeningStatus::from_storage_str("denied").is_err());
    assert!(ExportScreeningStatus::from_storage_str("").is_err());
}
