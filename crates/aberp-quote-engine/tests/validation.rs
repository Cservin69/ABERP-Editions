//! Input-validation tests — every `QuoteError` variant must be
//! reachable from a single bad-input nudge. If any of these turns
//! into "engine quietly produced nonsense", we've lost the trust
//! signal `[[trust-code-not-operator]]` depends on.

mod common;

use aberp_quote_engine::{
    quote, ComplexityRule, Feature, FeatureGraph, FeatureType, QuoteError, StockForm,
    ToleranceRange,
};
use common::*;

#[test]
fn material_grade_missing_in_catalogue_errors_loud() {
    let materials = vec![default_material("6061-T6")];
    let mut fg = simple_feature_graph("6061-T6");
    fg.material_grade = "MONEL_650_NOT_IN_CATALOGUE".to_string();
    let err = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .expect_err("missing grade must error");
    match err {
        QuoteError::MaterialNotInCatalogue { grade } => {
            assert_eq!(grade, "MONEL_650_NOT_IN_CATALOGUE")
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn tolerance_not_in_table_errors_loud() {
    let materials = vec![default_material("6061-T6")];
    let fg = simple_feature_graph("6061-T6");
    // Tolerance table empty — every lookup fails.
    let err = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &[],
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        ToleranceRange::Tight,
    )
    .expect_err("empty tolerance table must error");
    match err {
        QuoteError::ToleranceNotInTable { tolerance } => assert_eq!(tolerance, "tight"),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn missing_complexity_rule_for_feature_errors_loud() {
    let materials = vec![default_material("6061-T6")];
    let fg = FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        bounding_box_mm: [10.0, 10.0, 10.0],
        volume_mm3: 1000.0,
        surface_area_mm2: 600.0,
        material_grade: "6061-T6".to_string(),
        features: vec![Feature {
            feature_type: FeatureType::Engraving,
            count: 1,
            representative_size_mm: 5.0, // XS
        }],
        requires_5_axis: false,
        thin_wall_present: false,
        stock_form: StockForm::RectangularBlock,
    };
    // Rules table covers only (pocket, M) — engraving/XS is missing.
    let rules = vec![ComplexityRule {
        id: 1,
        feature_type: "pocket".to_string(),
        size_bucket: "M".to_string(),
        count_min: 1,
        count_max: None,
        base_time_minutes: 2.0,
        multiplier: 1.0,
        setup_penalty_minutes: 5.0,
    }];
    let err = quote(
        &fg,
        &materials,
        &rules,
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .expect_err("no rule must error");
    match err {
        QuoteError::NoComplexityRuleForFeature {
            feature_type,
            size_bucket,
            count,
        } => {
            assert_eq!(feature_type, "engraving");
            assert_eq!(size_bucket, "XS");
            assert_eq!(count, 1);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn inverted_count_bounds_in_rule_errors_loud() {
    let materials = vec![default_material("6061-T6")];
    let fg = simple_feature_graph("6061-T6");
    let rules = vec![ComplexityRule {
        id: 42,
        feature_type: "hole".to_string(),
        size_bucket: "XS".to_string(),
        count_min: 10,
        count_max: Some(2),
        base_time_minutes: 2.0,
        multiplier: 1.0,
        setup_penalty_minutes: 0.0,
    }];
    let err = quote(
        &fg,
        &materials,
        &rules,
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .expect_err("inverted bounds must error");
    match err {
        QuoteError::InvalidComplexityRule { rule_id, .. } => assert_eq!(rule_id, 42),
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn quantity_zero_errors_loud() {
    let materials = vec![default_material("6061-T6")];
    let fg = simple_feature_graph("6061-T6");
    let err = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        0,
        DEFAULT_TOL,
    )
    .expect_err("qty=0 must error");
    assert!(matches!(err, QuoteError::QuantityZero));
}

#[test]
fn margin_floor_violation_refused() {
    let materials = vec![default_material("6061-T6")];
    let fg = simple_feature_graph("6061-T6");
    let mut params = default_parameters();
    // Force a price floor that the standard inputs can't satisfy.
    // profit_margin_base=0.35 ⇒ actual margin/total is fixed by
    // overhead/margin algebra at ~0.30; set floor above that.
    params.profit_margin_base = 0.05; // → actual margin/total ≈ 0.0476
    params.min_margin = 0.20;
    let err = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &params,
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .expect_err("margin floor must refuse");
    match err {
        QuoteError::MarginFloorViolation {
            actual_pct,
            floor_pct,
            ..
        } => {
            assert!(actual_pct < floor_pct);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn newer_schema_version_refused() {
    let materials = vec![default_material("6061-T6")];
    let mut fg = simple_feature_graph("6061-T6");
    fg.schema_version = FeatureGraph::SCHEMA_VERSION + 1;
    let err = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .expect_err("future schema must refuse");
    assert!(matches!(err, QuoteError::UnsupportedSchemaVersion { .. }));
}
