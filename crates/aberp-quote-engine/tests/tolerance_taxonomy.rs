//! ADR-0097 Part 1 (T2) — tolerance-taxonomy normalisation unit tests.
//!
//! Each professional drawing dialect ([`ToleranceSpec`]) maps deterministically
//! onto the internal 5-band [`ToleranceRange`] via [`tightness`] /
//! [`normalize_tolerance`]. These pin the Q4 band edges, the size-aware ISO 286
//! `±` derivation, and the Q5 per-drawing manual-review flag.
//!
//! **No pricing here.** T2 moves no quote number; the golden / determinism /
//! branch / property tests are the inert-by-default tripwire. This file proves
//! the *new* math the defaults keep dormant.

use aberp_quote_engine::{
    normalize_tolerance, tightness, GeneralClass, ToleranceRange, ToleranceSpec,
    IT_GRADE_PRECISION_MAX, IT_GRADE_STANDARD_MAX, IT_GRADE_TIGHT_MAX,
    IT_GRADE_ULTRA_PRECISION_MAX,
};

fn gc(class: GeneralClass) -> ToleranceSpec {
    ToleranceSpec::GeneralClass { class }
}

// ── ISO 2768 general class → band (Q4 map) ──────────────────────────────────

#[test]
fn iso2768_medium_maps_to_standard_the_byte_identical_default() {
    // The universal title-block default ⇒ Standard ⇒ today's behaviour.
    assert_eq!(
        tightness(gc(GeneralClass::Iso2768Medium), 0.0),
        ToleranceRange::Standard
    );
}

#[test]
fn iso2768_fine_coarse_verycoarse_bands() {
    assert_eq!(
        tightness(gc(GeneralClass::Iso2768Fine), 0.0),
        ToleranceRange::Tight
    );
    assert_eq!(
        tightness(gc(GeneralClass::Iso2768Coarse), 0.0),
        ToleranceRange::Loose
    );
    assert_eq!(
        tightness(gc(GeneralClass::Iso2768VeryCoarse), 0.0),
        ToleranceRange::Loose
    );
}

// ── IT grade → band (Q4 edges) ──────────────────────────────────────────────

#[test]
fn it_grade_band_edges_cover_the_whole_q4_map() {
    let band = |g| tightness(ToleranceSpec::ItGrade { grade: g }, 0.0);
    // ≤ IT5 → UltraPrecision
    assert_eq!(band(1), ToleranceRange::UltraPrecision);
    assert_eq!(band(5), ToleranceRange::UltraPrecision);
    // IT6–IT7 → Precision
    assert_eq!(band(6), ToleranceRange::Precision);
    assert_eq!(band(7), ToleranceRange::Precision);
    // IT8–IT9 → Tight
    assert_eq!(band(8), ToleranceRange::Tight);
    assert_eq!(band(9), ToleranceRange::Tight);
    // IT10–IT11 → Standard
    assert_eq!(band(10), ToleranceRange::Standard);
    assert_eq!(band(11), ToleranceRange::Standard);
    // IT12–IT14 → Loose (and saturating beyond)
    assert_eq!(band(12), ToleranceRange::Loose);
    assert_eq!(band(14), ToleranceRange::Loose);
    assert_eq!(band(18), ToleranceRange::Loose);
}

#[test]
fn it7_maps_to_precision_per_plan() {
    assert_eq!(
        tightness(ToleranceSpec::ItGrade { grade: 7 }, 0.0),
        ToleranceRange::Precision
    );
}

#[test]
fn q4_band_edge_constants_are_pinned() {
    // Golden-guarded constants — a silent edge change breaks this.
    assert_eq!(IT_GRADE_ULTRA_PRECISION_MAX, 5);
    assert_eq!(IT_GRADE_PRECISION_MAX, 7);
    assert_eq!(IT_GRADE_TIGHT_MAX, 9);
    assert_eq!(IT_GRADE_STANDARD_MAX, 11);
}

// ── explicit ± → size-aware ISO 286 IT grade → band ─────────────────────────

#[test]
fn plus_minus_anchor_phi10_is_it6_precision() {
    // Plan's binding anchor: ±0.01 @ Ø10 → IT6 → Precision.
    let n = normalize_tolerance(ToleranceSpec::PlusMinus { value_mm: 0.01 }, 10.0);
    assert_eq!(n.band, ToleranceRange::Precision);
    assert!(
        n.reason.contains("IT6"),
        "reason should name IT6: {}",
        n.reason
    );
    assert!(!n.manual_review);
}

#[test]
fn plus_minus_is_size_aware_same_tolerance_different_band() {
    // Correct ISO 286 direction: the SAME ± is a TIGHTER grade on a LARGER
    // nominal (grade values scale up with size). ±0.05: Ø20 → IT9 → Tight;
    // Ø200 → IT7 → Precision.
    let small = tightness(ToleranceSpec::PlusMinus { value_mm: 0.05 }, 20.0);
    let large = tightness(ToleranceSpec::PlusMinus { value_mm: 0.05 }, 200.0);
    assert_eq!(small, ToleranceRange::Tight);
    assert_eq!(large, ToleranceRange::Precision);
    assert!(
        large > small,
        "a larger nominal at the same ± must resolve tighter"
    );
}

#[test]
fn plus_minus_phi250_is_tighter_not_looser_iso286_flagged() {
    // FLAGGED deviation from the plan's prose ("±0.01@Ø250→looser").
    //
    // ISO 286: a *fixed* ± is HARDER to hold on a LARGER diameter (tighter
    // grade), so ±0.01@Ø250 derives to ≤IT5 ⇒ UltraPrecision — TIGHTER than
    // the Ø10 Precision result, NOT looser. This is the professionally-correct,
    // no-silent-under-quote direction the ADR mandates. Zero price impact in T2
    // (the band is not yet costed); operator-overridable per job/feature.
    let phi10 = tightness(ToleranceSpec::PlusMinus { value_mm: 0.01 }, 10.0);
    let phi250 = tightness(ToleranceSpec::PlusMinus { value_mm: 0.01 }, 250.0);
    assert_eq!(phi10, ToleranceRange::Precision);
    assert_eq!(phi250, ToleranceRange::UltraPrecision);
    assert!(
        phi250 > phi10,
        "fixed ± on a larger nominal is tighter, not looser"
    );
}

// ── "per drawing" → default band + manual-review flag (Q5) ───────────────────

#[test]
fn per_drawing_flags_manual_review_without_silent_tightening() {
    let n = normalize_tolerance(ToleranceSpec::PerDrawing, 12.0);
    assert_eq!(
        n.band,
        ToleranceRange::Standard,
        "per-drawing must resolve to the default band, never silently tight/loose"
    );
    assert!(
        n.manual_review,
        "per-drawing must raise the manual-review flag"
    );
    assert!(
        n.reason.contains("MANUAL REVIEW"),
        "reason should state manual review: {}",
        n.reason
    );
    assert!(ToleranceSpec::PerDrawing.requires_manual_review());
}

#[test]
fn only_per_drawing_requires_review() {
    for spec in [
        ToleranceSpec::Unspecified,
        gc(GeneralClass::Iso2768Fine),
        ToleranceSpec::ItGrade { grade: 6 },
        ToleranceSpec::PlusMinus { value_mm: 0.01 },
    ] {
        assert!(
            !normalize_tolerance(spec, 10.0).manual_review,
            "{spec:?} must not require review"
        );
    }
    assert!(normalize_tolerance(ToleranceSpec::PerDrawing, 10.0).manual_review);
}

// ── Unspecified → inert default ─────────────────────────────────────────────

#[test]
fn unspecified_is_inert_default_band_and_no_review() {
    let n = normalize_tolerance(ToleranceSpec::Unspecified, 0.0);
    assert_eq!(n.band, ToleranceRange::Standard);
    assert!(!n.manual_review);
    assert!(ToleranceSpec::Unspecified.is_unspecified());
    assert!(!gc(GeneralClass::Iso2768Medium).is_unspecified());
}

// ── determinism (the reasoning line is the trust signal) ────────────────────

#[test]
fn normalisation_is_deterministic_byte_identical() {
    let a = normalize_tolerance(ToleranceSpec::PlusMinus { value_mm: 0.02 }, 80.0);
    let b = normalize_tolerance(ToleranceSpec::PlusMinus { value_mm: 0.02 }, 80.0);
    assert_eq!(a, b);
    assert!(a.reason.contains("ISO 286 size-aware"));
}

// ── serde round-trip of the wire contract ───────────────────────────────────

#[test]
fn tolerance_spec_serde_round_trips_every_dialect() {
    for spec in [
        ToleranceSpec::Unspecified,
        gc(GeneralClass::Iso2768Fine),
        gc(GeneralClass::Iso2768Medium),
        ToleranceSpec::ItGrade { grade: 7 },
        ToleranceSpec::PlusMinus { value_mm: 0.01 },
        ToleranceSpec::PerDrawing,
    ] {
        let j = serde_json::to_string(&spec).expect("serialize");
        let back: ToleranceSpec = serde_json::from_str(&j).expect("deserialize");
        assert_eq!(spec, back, "round-trip failed for {j}");
    }
    // Spot-check the tagged wire strings (the S269 contract).
    assert_eq!(
        serde_json::to_string(&ToleranceSpec::ItGrade { grade: 7 }).unwrap(),
        r#"{"kind":"it_grade","grade":7}"#
    );
    assert_eq!(
        serde_json::to_string(&gc(GeneralClass::Iso2768Medium)).unwrap(),
        r#"{"kind":"general_class","class":"iso2768_medium"}"#
    );
}
