//! Catalogue + tunables snapshot types — the **monetary** side of
//! the engine input. The wiring layer (S271) reads the four DB tables
//! S266/S267 shipped and constructs these snapshots; the engine
//! treats them as immutable inputs.
//!
//! Field shapes here mirror the DB columns (S266
//! `quoting_materials`, S267 `quoting_complexity_rules` /
//! `quoting_tolerance_multipliers` / `quoting_parameters` /
//! `quoting_stock_adjustments`). When the wiring layer adds
//! engine-only fields (e.g. `machining_rate_eur_per_minute` does not
//! yet have a DB home — see the lib.rs pushback list), this is where
//! they land.

use serde::{Deserialize, Serialize};

/// Closed-vocab stock state — verbatim from S266
/// `quoting_materials.stock_status`. The engine reads this to apply
/// the [`StockAdjustment`] row, and the downstream PDF/SPA use it
/// for the stale-stock banner (design §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StockStatus {
    /// On the shelf today.
    InStock,
    /// Sourceable in 1–2 days.
    Source1_2d,
    /// Sourceable in 3–7 days.
    Source3_7d,
    /// Special order — long lead, exotic, or vendor-specific.
    SpecialOrder,
}

impl StockStatus {
    /// DB storage string — matches `quoting_materials.stock_status` /
    /// `quoting_stock_adjustments.stock_status`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::InStock => "in_stock",
            Self::Source1_2d => "source_1_2d",
            Self::Source3_7d => "source_3_7d",
            Self::SpecialOrder => "special_order",
        }
    }
}

/// A row from `quoting_materials` (S266) — exactly the columns the
/// engine reads. Density and cost together produce mass × EUR/kg →
/// material cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Material {
    /// Natural key — same as `quoting_materials.grade`.
    pub grade: String,
    /// Grams per cubic centimetre.
    pub density_g_cm3: f64,
    /// EUR per kilogram.
    pub cost_per_kg_eur: f64,
    /// >1 = easier than baseline (faster), <1 = harder (slower). Used
    /// as a divisor on machining minutes.
    pub machinability_index: f64,
    /// Operator override knob on top of all other multipliers.
    pub quote_multiplier: f64,
    /// Drives the [`StockAdjustment`] lookup.
    pub stock_status: StockStatus,
}

/// A row from `quoting_complexity_rules` (S267). The engine matches
/// every input [`crate::Feature`] to the *most specific* rule for its
/// `(feature_type, size_bucket, count)` triple and adds the rule's
/// `setup_penalty_minutes` once per **distinct rule** that fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComplexityRule {
    /// Stable identity — used to dedupe "rule fired once" for
    /// setup-penalty accumulation. Must be unique within a snapshot.
    pub id: i64,
    /// Matches [`crate::FeatureType::as_db_str`].
    pub feature_type: String,
    /// Matches [`crate::SizeBucket::as_db_str`].
    pub size_bucket: String,
    /// Inclusive lower bound on the feature count.
    pub count_min: u32,
    /// Inclusive upper bound. `None` = open-ended (catch-all rule).
    pub count_max: Option<u32>,
    /// Time per single feature occurrence.
    pub base_time_minutes: f64,
    /// Multiplier on (base_time × count).
    pub multiplier: f64,
    /// Setup penalty added ONCE when this rule fires for any feature,
    /// no matter how many features share it.
    pub setup_penalty_minutes: f64,
}

/// A row from `quoting_tolerance_multipliers` (S267).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToleranceMultiplier {
    /// Matches [`crate::ToleranceRange::as_db_str`].
    pub tolerance_range: String,
    /// Applied to `labor_cost` (engine step 6).
    pub multiplier: f64,
    /// Added to `inspection_minutes` once per feature in the graph.
    pub inspection_minutes_per_feature: f64,
}

/// The singleton `quoting_parameters` row (S267) — the engine's
/// global knobs. The mapping from the live DB row to this struct is
/// the wiring layer's job (S271).
///
/// **One v1 knob that does NOT have a DB column yet:**
/// [`Self::machining_rate_eur_per_minute`]. The S267 table omits it;
/// the design doc §7 names a future `quoting_machines.hourly_rate`
/// split. For now S271 either adds a column or hardcodes a constant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotingParameters {
    /// E.g. 0.08 = 8% material over-allowance.
    pub scrap_factor: f64,
    /// E.g. 0.35 = 35% margin on (subtotal + overhead).
    pub profit_margin_base: f64,
    /// E.g. 0.20 = 20% overhead on subtotal.
    pub overhead_factor: f64,
    /// At or above this quantity, setup is amortised across the qty;
    /// below it, the full setup cost is charged on every part.
    pub setup_amortization_threshold: u32,
    /// Minimum margin / total_price ratio. Below this the engine
    /// errors [`crate::QuoteError::MarginFloorViolation`] — better to
    /// refuse than to quote a money-losing job.
    pub min_margin: f64,
    /// Fractional surcharge applied to material cost when the grade
    /// matches an exotic substring (Inconel / Titanium for v1).
    pub exotic_material_tax: f64,
    /// EUR per minute of machine + operator time. Engine multiplies
    /// (machining + inspection) minutes by this.
    pub machining_rate_eur_per_minute: f64,
}

/// A row from `quoting_stock_adjustments` (S267) — ±% price tweak
/// keyed on `(grade, stock_status)`. Engine applies it multiplicatively
/// to the base material cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StockAdjustment {
    /// Matches `quoting_materials.grade` exactly.
    pub grade: String,
    /// Matches the material's current `stock_status`.
    pub stock_status: String,
    /// Signed fractional adjustment: 0.10 = +10%, -0.05 = -5%.
    pub price_adjustment_pct: f64,
}
