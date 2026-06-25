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
    /// Physically-correct per-material time **multiplier** (>1 = harder/
    /// slower to cut, <1 = softer/faster). 6061-T6 = 1.0 reference;
    /// PEEK ≈ 0.8, Ti-6Al-4V ≈ 3.5, Inconel 718 ≈ 5.0. The S418
    /// geometry model multiplies roughing + finishing minutes by this
    /// (see `engine.rs` §5). REPLACES the pre-S418 `machinability_index`
    /// divisor, whose seed values were semantically inverted (Inconel
    /// 5.0 read as "5× faster") — see the report §6.1. The inverted
    /// field is deleted (rule 13), not kept alongside (rule 7).
    pub machining_difficulty: f64,
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
    /// Applied to the machining cost (engine step 5).
    pub multiplier: f64,
    /// Added to `inspection_minutes` once per feature in the graph.
    pub inspection_minutes_per_feature: f64,
}

/// The singleton `quoting_parameters` row (S267, extended S418) — the
/// engine's global knobs. The mapping from the live DB row to this
/// struct is the wiring layer's job (S271 / `quote_pricing_pipeline`).
///
/// S418 promoted the geometry-driven machining model (report §5/§8):
/// the rate moved off the pipeline hardcode into the DB, and six new
/// knobs (`cad_cam_*`, `mrr_rough_ref_*`, `t_finish_*`, `setup_*_min`)
/// landed so the operator tunes the whole model from Settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotingParameters {
    /// Stock-oversize fraction (S418 repurpose): the machined part is
    /// cut from a block sized `bbox × (1 + scrap_factor)`. Drives BOTH
    /// the material billing (on stock volume, report §6.4) and the
    /// roughing-removal volume (report §5.1). E.g. 0.15 = 15% oversize.
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
    /// (machining + inspection) minutes by this. Day-1 = 1.6667
    /// (= 100 EUR/machine-hour, report §8.1).
    pub machining_rate_eur_per_minute: f64,
    /// EUR per hour of CAD-CAM programming / fixturing. Day-1 = 100.
    /// The one-time design cost (report §4) is amortised across qty.
    pub cad_cam_rate_eur_per_hour: f64,
    /// Floor of the CAD-CAM hour estimate (report §4.1 `base`). Day-1
    /// = 1.0 — every part costs at least one programming hour.
    pub cad_cam_base_hours: f64,
    /// Reference roughing material-removal rate at difficulty 1.0, in
    /// cm³/min (report §5.2). Effective rate = this ÷ machining
    /// difficulty. Day-1 = 8.0.
    pub mrr_rough_ref_cm3_per_min: f64,
    /// Finishing-pass time per cm² of surface area, at difficulty 1.0,
    /// in min/cm² (report §5.2). Day-1 = 0.08.
    pub t_finish_min_per_cm2: f64,
    /// Fixed per-job setup minutes (fixturing + tool-load + tryout),
    /// report §5.5. Day-1 = 20.
    pub setup_base_min: f64,
    /// Extra setup minutes when the part routes to a 5-axis machine
    /// (report §5.5). Day-1 = 25.
    pub setup_5axis_min: f64,
    /// Largest bar-stock diameter (mm) the shop's bar-fed Swiss/turn-mill
    /// accepts. A turned/round blank with `od <= bar_capacity_mm` routes to
    /// the lights-out [`crate::MachineFamily::SwissTurnMill`]; a larger round
    /// routes to [`crate::MachineFamily::TurnMill`] (ADR-0094 Gap 2 routing).
    /// `#[serde(default)]` (32.0) so pre-ADR-0094 `quoting_parameters` rows
    /// that lack the column still deserialize and price exactly as today.
    #[serde(default = "default_bar_capacity_mm")]
    pub bar_capacity_mm: f64,
}

/// Serde default for [`QuotingParameters::bar_capacity_mm`] — pre-ADR-0094
/// persisted parameter rows predate the column. 32 mm is a common bar-feeder
/// capacity (ADR-0094 Gap 2 proposed default).
fn default_bar_capacity_mm() -> f64 {
    32.0
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

/// A row from the `quoting_machine_rates` catalogue table (ADR-0094 Gap 2,
/// wired in S4). Keyed by [`crate::MachineFamily`] (via its `as_db_str`
/// round-trip), it attaches the family's true EUR/min to the engine so a
/// bar-fed Swiss running lights-out can price a small turned part below an
/// attended 3-axis mill. Mirrors the snapshot shape of the other catalogue
/// rows: the wiring layer reads the DB table, the engine treats
/// `&[MachineRate]` as an immutable input. An **empty** slice (or no row for
/// the routed family) ⇒ the engine falls back to the global
/// [`QuotingParameters::machining_rate_eur_per_minute`] — byte-identical to
/// pre-ADR-0094 pricing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineRate {
    /// Matches [`crate::MachineFamily::as_db_str`] (e.g. `"swiss-turn-mill"`).
    pub family: String,
    /// The family's attended EUR/min — the rate when a dedicated operator
    /// tends the machine.
    pub attended_rate_eur_per_min: f64,
    /// Multiplier in (0, 1] applied to the attended rate when the job runs
    /// unattended (lights-out): effective EUR/min = attended * this. Only
    /// applied when `unattended_capable` AND the job qualifies (turned part
    /// on bar stock at/above the setup-amortization quantity).
    pub lights_out_factor: f64,
    /// Whether this family can run unattended (bar-fed Swiss/turn-mill =
    /// true; a manual mill = false).
    pub unattended_capable: bool,
}

/// A row from the `quoting_gear_processes` catalogue table (ADR-0094 Gap 3,
/// wired in S6). Keyed by [`crate::GearProcess::as_db_str`], it carries the
/// per-process time coefficients so the gear-generation **math** stays pure
/// in the engine while the **numbers** stay operator-tunable — mirroring the
/// snapshot shape of the other catalogue rows. The wiring layer reads the DB
/// table; the engine treats `&[GearProcessRate]` as an immutable input. An
/// **empty** slice (or no row for a gear's selected process) ⇒ that gear
/// contributes **zero** cost and a loud reasoning-log line. With no gears at
/// all the gear path is never entered ⇒ pricing is byte-identical to
/// pre-ADR-0094-Gap-3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GearProcessRate {
    /// Matches [`crate::GearProcess::as_db_str`] (e.g. `"power_skive"`).
    pub process: String,
    /// Indexing / tool-load minutes charged once per gear.
    pub setup_min: f64,
    /// Base generation minutes per tooth (before module / face-width /
    /// quality scaling).
    pub min_per_tooth: f64,
    /// Generation time scales with `module_mm^module_exponent` — bigger teeth
    /// cut slower.
    pub module_exponent: f64,
    /// Quality-factor growth per AGMA class above the datum
    /// [`crate::GEAR_AGMA_DATUM_CLASS`]: `quality_factor = 1 + max(0, agma -
    /// datum) * this`.
    pub agma_quality_factor_base: f64,
    /// Multiplier (< 1) applied when the process runs in-cycle on the routed
    /// turning family (power-skiving on a Swiss/turn-mill — no second op, no
    /// refixture). `1.0` for a standalone op.
    pub in_cycle_factor: f64,
}
