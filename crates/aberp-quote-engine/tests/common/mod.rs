//! Shared test fixtures — a sensible default snapshot every test can
//! mutate locally without re-typing twelve fields. Mirrors the seed
//! data shape that S266/S267 will install on a fresh tenant.

#![allow(dead_code)]

use aberp_quote_engine::{
    CatalogueSnapshot, ComplexityRule, Feature, FeatureGraph, FeatureType, GearProcessRate,
    MachineRate, Material, QuotingParameters, StockAdjustment, StockForm, StockStatus,
    ToleranceCostRate, ToleranceMultiplier, ToleranceRange, ToleranceSpec,
};

pub fn default_material(grade: &str) -> Material {
    Material {
        grade: grade.to_string(),
        density_g_cm3: 2.70, // ~6061-T6
        cost_per_kg_eur: 8.0,
        machining_difficulty: 1.0, // 6061-T6 reference (S418)
        quote_multiplier: 1.0,
        stock_status: StockStatus::InStock,
    }
}

pub fn exotic_material(grade: &str) -> Material {
    Material {
        grade: grade.to_string(),
        density_g_cm3: 8.19, // Inconel 718
        cost_per_kg_eur: 65.0,
        machining_difficulty: 5.0, // Inconel 718 — hardest (S418)
        quote_multiplier: 1.0,
        stock_status: StockStatus::SpecialOrder,
    }
}

/// S418 day-1 parameter set (report §8.1). The engine tests use the
/// real production knobs so the golden, branch, and benchmark tests
/// all exercise the shipped model — one source of truth.
pub fn default_parameters() -> QuotingParameters {
    QuotingParameters {
        scrap_factor: 0.15, // stock-oversize (repurposed, report §6.4)
        profit_margin_base: 0.35,
        overhead_factor: 0.20,
        setup_amortization_threshold: 5,
        min_margin: 0.10,
        exotic_material_tax: 0.05,
        machining_rate_eur_per_minute: 1.6667, // 100 EUR/machine-hour
        cad_cam_rate_eur_per_hour: 100.0,
        cad_cam_base_hours: 1.0,
        mrr_rough_ref_cm3_per_min: 8.0,
        t_finish_min_per_cm2: 0.08,
        setup_base_min: 20.0,
        setup_5axis_min: 25.0,
        bar_capacity_mm: 32.0, // ADR-0094 Gap 2 default; inert for RectangularBlock
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
        tolerance: ToleranceSpec::Unspecified,
        critical_feature_tolerances: Vec::new(),
        bounding_box_mm: [50.0, 30.0, 20.0],
        volume_mm3: 25_000.0,
        // Left 0.0 so tests exercise the engine's bbox-area fallback
        // (report §5.4). The benchmark/property tests that need a real
        // area set it explicitly.
        surface_area_mm2: 0.0,
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
        // S1/ADR-0094: explicit default form keeps every existing golden,
        // determinism, branch and property number byte-identical.
        stock_form: StockForm::RectangularBlock,
        // S5/ADR-0094 Gap 3: explicit empty gears keeps every existing golden,
        // determinism, branch, machine-rate and property number byte-identical.
        gears: Vec::new(),
    }
}

pub const DEFAULT_QTY: u32 = 10;
pub const DEFAULT_TOL: ToleranceRange = ToleranceRange::Standard;

/// Owns the default catalogue `Vec`s so a test can hand the engine a
/// borrowed [`CatalogueSnapshot`] view (the snapshot borrows; the fixture
/// owns). The machine-rate and gear-process slices default empty — inert,
/// matching today's byte-identical pricing — and a test can push rows to
/// exercise a populated path.
pub struct CatalogueFixture {
    pub materials: Vec<Material>,
    pub complexity_rules: Vec<ComplexityRule>,
    pub tolerance_multipliers: Vec<ToleranceMultiplier>,
    pub stock_adjustments: Vec<StockAdjustment>,
    pub machine_rates: Vec<MachineRate>,
    pub gear_process_rates: Vec<GearProcessRate>,
    pub tolerance_cost_rates: Vec<ToleranceCostRate>,
}

impl CatalogueFixture {
    /// The default single-material catalogue the engine tests use, mirroring
    /// the positional `default_*` fixtures (one material, catch-all rules,
    /// the five tolerance bands, no stock adjustment, empty machine/gear
    /// slices).
    pub fn new(grade: &str) -> Self {
        Self {
            materials: vec![default_material(grade)],
            complexity_rules: catchall_complexity_rules(),
            tolerance_multipliers: default_tolerance_multipliers(),
            stock_adjustments: no_stock_adjustments(),
            machine_rates: Vec::new(),
            gear_process_rates: Vec::new(),
            tolerance_cost_rates: Vec::new(),
        }
    }

    /// Borrow a [`CatalogueSnapshot`] view over the owned `Vec`s.
    pub fn snapshot(&self) -> CatalogueSnapshot<'_> {
        CatalogueSnapshot {
            materials: &self.materials,
            complexity_rules: &self.complexity_rules,
            tolerance_multipliers: &self.tolerance_multipliers,
            stock_adjustments: &self.stock_adjustments,
            machine_rates: &self.machine_rates,
            gear_process_rates: &self.gear_process_rates,
            tolerance_cost_rates: &self.tolerance_cost_rates,
        }
    }
}
