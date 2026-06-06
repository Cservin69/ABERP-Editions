//! Shared test fixtures — a sensible default snapshot every test can
//! mutate locally without re-typing twelve fields. Mirrors the seed
//! data shape that S266/S267 will install on a fresh tenant.

#![allow(dead_code)]

use aberp_quote_engine::{
    ComplexityRule, Feature, FeatureGraph, FeatureType, Material, QuotingParameters,
    StockAdjustment, StockStatus, ToleranceMultiplier, ToleranceRange,
};

pub fn default_material(grade: &str) -> Material {
    Material {
        grade: grade.to_string(),
        density_g_cm3: 2.70, // ~6061-T6
        cost_per_kg_eur: 8.0,
        machinability_index: 1.2,
        quote_multiplier: 1.0,
        stock_status: StockStatus::InStock,
    }
}

pub fn exotic_material(grade: &str) -> Material {
    Material {
        grade: grade.to_string(),
        density_g_cm3: 8.19, // Inconel 718
        cost_per_kg_eur: 65.0,
        machinability_index: 0.3,
        quote_multiplier: 1.0,
        stock_status: StockStatus::SpecialOrder,
    }
}

pub fn default_parameters() -> QuotingParameters {
    QuotingParameters {
        scrap_factor: 0.08,
        profit_margin_base: 0.35,
        overhead_factor: 0.20,
        setup_amortization_threshold: 5,
        min_margin: 0.10,
        exotic_material_tax: 0.05,
        machining_rate_eur_per_minute: 1.50, // ~90 EUR/hr
    }
}

pub fn default_tolerance_multipliers() -> Vec<ToleranceMultiplier> {
    vec![
        ToleranceMultiplier {
            tolerance_range: "loose".to_string(),
            multiplier: 0.9,
            inspection_minutes_per_feature: 0.0,
        },
        ToleranceMultiplier {
            tolerance_range: "standard".to_string(),
            multiplier: 1.0,
            inspection_minutes_per_feature: 0.0,
        },
        ToleranceMultiplier {
            tolerance_range: "tight".to_string(),
            multiplier: 1.4,
            inspection_minutes_per_feature: 0.5,
        },
        ToleranceMultiplier {
            tolerance_range: "precision".to_string(),
            multiplier: 1.9,
            inspection_minutes_per_feature: 1.5,
        },
        ToleranceMultiplier {
            tolerance_range: "ultra_precision".to_string(),
            multiplier: 2.8,
            inspection_minutes_per_feature: 3.0,
        },
    ]
}

/// One catch-all rule per (feature_type, size_bucket) so any feature
/// can match. Test cases override individual rules where the
/// assertion matters.
pub fn catchall_complexity_rules() -> Vec<ComplexityRule> {
    let mut rules = Vec::new();
    let mut id = 1_i64;
    let feature_types = [
        "pocket",
        "hole",
        "slot",
        "thread",
        "undercut_5axis",
        "thin_wall",
        "surface",
        "engraving",
    ];
    let buckets = ["XS", "S", "M", "L", "XL"];
    for ft in feature_types {
        for sb in buckets {
            rules.push(ComplexityRule {
                id,
                feature_type: ft.to_string(),
                size_bucket: sb.to_string(),
                count_min: 1,
                count_max: None,
                base_time_minutes: 2.0,
                multiplier: 1.0,
                setup_penalty_minutes: 5.0,
            });
            id += 1;
        }
    }
    rules
}

pub fn no_stock_adjustments() -> Vec<StockAdjustment> {
    Vec::new()
}

pub fn simple_feature_graph(grade: &str) -> FeatureGraph {
    FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        bounding_box_mm: [50.0, 30.0, 20.0],
        volume_mm3: 25_000.0,
        material_grade: grade.to_string(),
        features: vec![
            Feature {
                feature_type: FeatureType::Hole,
                count: 4,
                representative_size_mm: 6.0, // XS
            },
            Feature {
                feature_type: FeatureType::Pocket,
                count: 1,
                representative_size_mm: 20.0, // S
            },
        ],
        requires_5_axis: false,
        thin_wall_present: false,
    }
}

pub const DEFAULT_QTY: u32 = 10;
pub const DEFAULT_TOL: ToleranceRange = ToleranceRange::Standard;
