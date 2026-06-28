//! The scoring function — the deterministic, geometry-driven pricing
//! algorithm (S418 overhaul of the original design-doc §10 scorer).
//!
//! Every step appends one line to `reasoning_log`. Reading the log
//! top-to-bottom reconstructs the price exactly. There is no hidden
//! contribution. The pipeline: material on stock volume → geometry
//! machining time (roughing + finishing, scaled by `machining_difficulty`)
//! → inspection → machining cost → setup → CAD-CAM design → overhead →
//! margin → min-margin gate.

use crate::breakdown::QuoteBreakdown;
use crate::capacity::MachineFamily;
use crate::catalogue::{
    ComplexityRule, GearProcessRate, MachineRate, Material, QuotingParameters, StockAdjustment,
    ToleranceCostRate, ToleranceMultiplier,
};
use crate::error::QuoteError;
use crate::feature_graph::{
    FeatureGraph, GearKind, GearOp, GearProcess, GeneralClass, SizeBucket, StockForm,
    ToleranceRange, ToleranceSpec,
};
use crate::ENGINE_VERSION;

/// Machining-cost multiplier applied when the part has a thin wall AND
/// the target tolerance is `Tight` or higher. Pinned as a constant here
/// so the golden test catches any drift.
///
/// `TODO`: when a future cut adds a per-machine-class rate split, this
/// could migrate to the `quoting_parameters` row alongside the rate.
pub const THIN_WALL_TIGHT_TOL_BUMP: f64 = 1.15;

// ── ADR-0094 Gap 3: gear-generation op constants (golden-guarded) ─────
//
// Pinned engine constants — like `THIN_WALL_TIGHT_TOL_BUMP`, the golden gear
// fixtures catch any drift. They encode the SHAPE of the cost model (datum
// quality class, face-width reference, the internal-ring shape→wire-EDM
// escalation); the per-process NUMBERS live in the operator-tunable
// `GearProcessRate` catalogue.

/// AGMA datum class for gear quality scaling: `quality_factor = 1 + max(0,
/// agma - this) * rate.agma_quality_factor_base`. At/below the datum the
/// quality factor is `1.0`; each class above adds the rate's per-class growth.
pub const GEAR_AGMA_DATUM_CLASS: u8 = 8;

/// Reference face width (mm): `facewidth_factor = face_width_mm / this`
/// (linear — a 2x wider gear takes 2x the generation time).
pub const GEAR_FACEWIDTH_REF_MM: f64 = 10.0;

/// Internal-ring AGMA escalation: under [`GearProcess::Auto`], an internal
/// ring at a class STRICTLY ABOVE this routes to slow, precise wire-EDM;
/// at/below it routes to gear shaping.
pub const GEAR_INTERNAL_WIRE_EDM_AGMA: u8 = 12;

// ── CAD-CAM complexity-matrix weights (report §4.2) ──────────────────
//
// `cad_cam_hours = clamp(1, base + Σ signal·weight, 5)`. The base hour
// is an operator-tunable parameter (`cad_cam_base_hours`); the signal
// weights below are pinned engine constants — the golden test catches
// any drift, and they encode a calibration decision (report §7) that is
// a code change, not a per-tenant knob.

/// 5-axis routing ⇒ programming-complexity premium (report §4.2).
const CAD_CAM_5AXIS_HOURS: f64 = 1.5;
/// Deep concavity (`fill_ratio < 0.30`) ⇒ heavy 3D pocketing strategy.
const CAD_CAM_LOW_FILL_HOURS: f64 = 1.0;
/// Moderate pocketing (`0.30 ≤ fill_ratio < 0.60`).
const CAD_CAM_MED_FILL_HOURS: f64 = 0.5;
/// Thin walls ⇒ workholding + deflection planning.
const CAD_CAM_THIN_WALL_HOURS: f64 = 0.5;
/// Large envelope (`max(bbox) ≥ 200 mm`) ⇒ multi-setup fixturing.
const CAD_CAM_LARGE_ENVELOPE_HOURS: f64 = 0.5;
/// Hard material ⇒ tool-strategy iteration / sim time.
const CAD_CAM_HARD_MATERIAL_HOURS: f64 = 0.5;
/// Upper clamp on the auto-derived CAD-CAM hours (report §4.1).
const CAD_CAM_MAX_HOURS: f64 = 5.0;

/// `fill_ratio` below this ⇒ low-fill (deep-pocket) CAM signal.
const LOW_FILL_RATIO: f64 = 0.30;
/// `fill_ratio` below this (and ≥ [`LOW_FILL_RATIO`]) ⇒ medium-fill.
const MED_FILL_RATIO: f64 = 0.60;
/// `max(bbox)` at or above this (mm) ⇒ large-envelope CAM signal.
const LARGE_ENVELOPE_MM: f64 = 200.0;

/// `machining_difficulty` at or above this classifies a grade as a
/// "hard material" for the CAD-CAM matrix. The report §4.2 names the
/// set {Ti, Inconel, Monel, superalloy}; under the S418 difficulty
/// seed those are exactly the grades with difficulty ≥ 3.0 (Monel 3.0,
/// Ti 3.5, Inconel 5.0). Using the difficulty column — not the
/// `exotic_material_tax` substring set (Inconel/Titanium only) — is a
/// deliberate deviation from the report's word "exotic": it captures
/// Monel, which the tax substrings miss, matching the report's *named
/// set* exactly. Flagged in the S418 commit message.
const HARD_MATERIAL_DIFFICULTY_THRESHOLD: f64 = 3.0;

/// Fallback reference roughing rate (cm³/min) used only if a corrupt
/// snapshot hands the engine a non-positive `mrr_rough_ref` — keeps the
/// output finite (the property test requires it). The boot-time
/// catalogue validation (`quote_pricing_pipeline`) refuses such a
/// snapshot loud before any quote runs; this is defence-in-depth.
const MRR_ROUGH_REF_FALLBACK: f64 = 8.0;

/// Substrings that, when contained in `Material::grade` (case-
/// insensitive), classify the material as exotic and trigger the
/// `exotic_material_tax` surcharge.
///
/// `TODO(S271+)`: replace with an `is_exotic` column on
/// `quoting_materials` (S267 pushback #4 deferred this to S268; we
/// keep the constant here so the algorithm is whole — the wiring
/// layer can swap the source of truth without changing the engine).
const EXOTIC_GRADE_SUBSTRINGS: &[&str] = &["inconel", "titanium"];

/// Case-insensitive substring match against the engine's private
/// exotic-grade substring list (currently `"inconel"` and `"titanium"`).
/// Public so the wiring layer can mirror the same classification in
/// any pre-quote SPA preview.
pub fn is_exotic_material(grade: &str) -> bool {
    let g = grade.to_ascii_lowercase();
    EXOTIC_GRADE_SUBSTRINGS.iter().any(|sub| g.contains(sub))
}

/// Immutable, borrowed bundle of the per-tenant **catalogue snapshot**
/// slices the engine prices against — the monetary inputs the wiring layer
/// reads from the `quoting_*` DB tables and hands the scorer ([`Material`],
/// [`ComplexityRule`], [`ToleranceMultiplier`], [`StockAdjustment`],
/// [`MachineRate`], [`GearProcessRate`]).
///
/// Resolves **ADR-0094 Q2**: rather than grow the engine entry past its
/// 11-positional-argument ceiling (the next catalogue slice would be the
/// 12th), the catalogue slices collapse into this one struct. Every later
/// session then adds a snapshot *field*, not a positional argument — the
/// `ToleranceCostRate` slice (ADR-0097 Part 2 / T3) lands here next.
///
/// Purely a view: borrowing `&[T]` keeps the engine zero-copy and the wiring
/// the owner of the `Vec`s. `Copy` because it is just a bundle of
/// shared-reference slices; `Default` yields an all-empty snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct CatalogueSnapshot<'a> {
    /// `quoting_materials` rows. An empty slice ⇒
    /// [`QuoteError::MaterialNotInCatalogue`] for the graph's grade.
    pub materials: &'a [Material],
    /// `quoting_complexity_rules` rows.
    pub complexity_rules: &'a [ComplexityRule],
    /// `quoting_tolerance_multipliers` rows — the overall-class flat
    /// multiplier + per-feature inspection minutes (unchanged by this ADR).
    pub tolerance_multipliers: &'a [ToleranceMultiplier],
    /// `quoting_stock_adjustments` rows.
    pub stock_adjustments: &'a [StockAdjustment],
    /// `quoting_machine_rates` rows (ADR-0094 Gap 2). Empty (or no row for
    /// the routed family) ⇒ the global flat machining rate ⇒ today's price.
    pub machine_rates: &'a [MachineRate],
    /// `quoting_gear_processes` rows (ADR-0094 Gap 3). Empty ⇒ the gear path
    /// is never entered ⇒ zero gear cost ⇒ today's price.
    pub gear_process_rates: &'a [GearProcessRate],
    /// `quoting_tolerance_cost_rates` rows (ADR-0097 Part 2 / T3). Keyed by the
    /// governing [`ToleranceRange`] band (`as_db_str`). Empty ⇒ the additive
    /// `tolerance_cost` path is never entered ⇒ `tolerance_cost = 0.0`, no
    /// reasoning line ⇒ today's price. T1 reserved this slot; T3 fills it.
    pub tolerance_cost_rates: &'a [ToleranceCostRate],
}

// ─────────────────────────────────────────────────────────────────────────
// ADR-0097 Part 1 (T2) — tolerance taxonomy → internal tightness scale.
//
// A pure, deterministic, reasoning-logged normaliser mapping every drawing
// dialect ([`ToleranceSpec`]) onto the existing 5-band [`ToleranceRange`].
// PURE math only — no I/O, clock, RNG. The heuristics that decide *which*
// dialect a drawing used live in the extractor/wiring; the engine only
// evaluates an already-classified [`ToleranceSpec`]. **T2 contributes nothing
// to price** — `tightness` is not yet called by the scorer (the additive
// tolerance cost line is T3); it is the typed, tested contract T3/T4 build on.
// ─────────────────────────────────────────────────────────────────────────

/// IT-grade → band edge: grades **≤ this** map to
/// [`ToleranceRange::UltraPrecision`]. Pinned, golden-guarded constant
/// (ADR-0097 Q4); operator-overridable per job.
pub const IT_GRADE_ULTRA_PRECISION_MAX: u8 = 5;
/// IT-grade → band edge: grades above `ULTRA` up to **this** map to
/// [`ToleranceRange::Precision`] (IT6–IT7). Pinned (ADR-0097 Q4).
pub const IT_GRADE_PRECISION_MAX: u8 = 7;
/// IT-grade → band edge: grades above `PRECISION` up to **this** map to
/// [`ToleranceRange::Tight`] (IT8–IT9). Pinned (ADR-0097 Q4).
pub const IT_GRADE_TIGHT_MAX: u8 = 9;
/// IT-grade → band edge: grades above `TIGHT` up to **this** map to
/// [`ToleranceRange::Standard`] (IT10–IT11); grades above it map to
/// [`ToleranceRange::Loose`] (IT12–IT14). Pinned (ADR-0097 Q4).
pub const IT_GRADE_STANDARD_MAX: u8 = 11;

/// ADR-0097 Part 2 / T3 — grinding-escalation minutes charged **per critical
/// feature** when a [`ToleranceCostRate`] row sets `grinding_escalation` AND the
/// governing band is the tightest ([`ToleranceRange::UltraPrecision`]). Models
/// the second-operation grind (refixture + spark-out passes) the flat
/// multiplier cannot see; costed at the `Grinder` [`MachineRate`] (falling back
/// to the routed effective EUR/min if the shop seeds no grinder rate). Pinned,
/// golden-guarded constant (ADR-0097 Q4); the rate stays operator-tunable.
pub const GRINDING_ESCALATION_MIN_PER_CRITICAL_FEATURE: f64 = 12.0;

/// The outcome of normalising a [`ToleranceSpec`] onto the internal tightness
/// scale: the resolved [`ToleranceRange`] band, the manual-review flag (set
/// only by [`ToleranceSpec::PerDrawing`] — ADR-0097 Q5), and a deterministic
/// reasoning-log line documenting the derivation.
///
/// Carrying the flag in the result type (not just the band) is the T2
/// contract: T3/T4 surface `manual_review` in the breakdown / UI and gate
/// auto-send on it; the band feeds the (future, T3) additive tolerance cost.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedTolerance {
    /// The resolved internal tightness band.
    pub band: ToleranceRange,
    /// `true` iff the spec was [`ToleranceSpec::PerDrawing`] — route to a
    /// human; never silently priced as tight or loose.
    pub manual_review: bool,
    /// A deterministic, operator-readable reasoning line (the trust signal).
    pub reason: String,
}

/// ISO 286 grade multipliers (units of the standard tolerance factor `i`) for
/// IT5..=IT16 — used to derive the IT grade nearest a given tolerance width.
const IT_GRADE_MULTIPLIERS: [(u8, f64); 12] = [
    (5, 7.0),
    (6, 10.0),
    (7, 16.0),
    (8, 25.0),
    (9, 40.0),
    (10, 64.0),
    (11, 100.0),
    (12, 160.0),
    (13, 250.0),
    (14, 400.0),
    (15, 640.0),
    (16, 1000.0),
];

/// ISO 286-1 standard tolerance factor `i` (micrometres) for a nominal size:
/// `i = 0.45·∛D + 0.001·D`, with `D` the geometric mean √(lo·hi) of the
/// standard size segment containing `nominal_mm`. Reproduces the published
/// ISO 286 IT values to < 1 µm. Pure; pinned segment table. `nominal_mm ≤ 0`
/// or non-finite clamps to the smallest segment (defence-in-depth — the
/// extractor should never emit those).
fn iso286_tolerance_factor_um(nominal_mm: f64) -> f64 {
    // ISO 286-1 standard size segments (mm), upper-inclusive; `D` is the
    // geometric mean √(lo·hi). First segment uses √(1·3).
    const SEGMENTS: [(f64, f64); 13] = [
        (1.0, 3.0),
        (3.0, 6.0),
        (6.0, 10.0),
        (10.0, 18.0),
        (18.0, 30.0),
        (30.0, 50.0),
        (50.0, 80.0),
        (80.0, 120.0),
        (120.0, 180.0),
        (180.0, 250.0),
        (250.0, 315.0),
        (315.0, 400.0),
        (400.0, 500.0),
    ];
    let n = if nominal_mm.is_finite() && nominal_mm > 0.0 {
        nominal_mm
    } else {
        1.0
    };
    let (lo, hi) = SEGMENTS
        .iter()
        .copied()
        .find(|&(_, hi)| n <= hi)
        .unwrap_or((400.0, 500.0));
    let d = (lo * hi).sqrt();
    0.45 * d.cbrt() + 0.001 * d
}

/// Derive the ISO 286 IT grade nearest to a symmetric `± value_mm` on a
/// `nominal_mm` dimension. The ± half-width (µm) is compared against the
/// standard grade values `multiplier · i(nominal)`; the nearest grade wins,
/// ties breaking to the **tighter** (lower) grade — conservative, never
/// under-state tightness. Saturates at IT5 (tighter ⇒ IT5) and IT16.
///
/// **Size-aware (ISO 286):** the same ± is a *tighter* grade on a *larger*
/// nominal, because the grade values scale up with size.
fn derive_it_grade_from_plus_minus(value_mm: f64, nominal_mm: f64) -> u8 {
    let half_width_um = value_mm.abs() * 1000.0;
    let i = iso286_tolerance_factor_um(nominal_mm);
    let ratio = half_width_um / i;
    let mut best_grade = IT_GRADE_MULTIPLIERS[0].0;
    let mut best_dist = f64::INFINITY;
    for &(grade, mult) in &IT_GRADE_MULTIPLIERS {
        let dist = (mult - ratio).abs();
        // Strict `<` + ascending iteration ⇒ ties resolve to the tighter grade.
        if dist < best_dist {
            best_dist = dist;
            best_grade = grade;
        }
    }
    best_grade
}

/// Map an ISO 286 IT grade onto the internal [`ToleranceRange`] band, per the
/// pinned ADR-0097 Q4 edges (`≤IT5→Ultra`, `IT6–7→Precision`, `IT8–9→Tight`,
/// `IT10–11→Standard`, `IT12+→Loose`).
fn it_grade_to_band(grade: u8) -> ToleranceRange {
    if grade <= IT_GRADE_ULTRA_PRECISION_MAX {
        ToleranceRange::UltraPrecision
    } else if grade <= IT_GRADE_PRECISION_MAX {
        ToleranceRange::Precision
    } else if grade <= IT_GRADE_TIGHT_MAX {
        ToleranceRange::Tight
    } else if grade <= IT_GRADE_STANDARD_MAX {
        ToleranceRange::Standard
    } else {
        ToleranceRange::Loose
    }
}

/// Map an ISO 2768 general class onto the internal [`ToleranceRange`] band,
/// per the pinned ADR-0097 Q4 map (`fine→Tight`, `medium→Standard`,
/// `coarse→Loose`, `very-coarse→Loose`). Medium is the universal title-block
/// default ⇒ [`ToleranceRange::Standard`] ⇒ byte-identical to today.
fn general_class_to_band(class: GeneralClass) -> ToleranceRange {
    match class {
        GeneralClass::Iso2768Fine => ToleranceRange::Tight,
        GeneralClass::Iso2768Medium => ToleranceRange::Standard,
        GeneralClass::Iso2768Coarse => ToleranceRange::Loose,
        GeneralClass::Iso2768VeryCoarse => ToleranceRange::Loose,
    }
}

/// Normalise a [`ToleranceSpec`] onto the internal 5-band [`ToleranceRange`]
/// tightness scale — the **pure, deterministic** taxonomy normaliser
/// (ADR-0097 Part 1).
///
/// - [`ToleranceSpec::Unspecified`] ⇒ [`ToleranceRange::Standard`], the inert
///   today-equivalent band (the engine *defers* to the resolved
///   `target_tolerance` for Unspecified — see [`normalize_tolerance`] — but
///   this total function returns the matching default band).
/// - [`ToleranceSpec::GeneralClass`] ⇒ ISO 2768 map.
/// - [`ToleranceSpec::ItGrade`] ⇒ IT-grade → band map.
/// - [`ToleranceSpec::PlusMinus`] ⇒ size-aware ISO 286 derivation then the
///   IT-grade map; `nominal_mm` is the feature's `representative_size_mm` (or
///   the part's, for a whole-part ±).
/// - [`ToleranceSpec::PerDrawing`] ⇒ [`ToleranceRange::Standard`] (default
///   band) — and the caller MUST honour the manual-review flag from
///   [`normalize_tolerance`]; never silently tightened (ADR-0097 Q5).
///
/// > **Flagged deviation from the plan's illustrative example.** The plan's T2
/// > note reads "±0.01@Ø10→IT6→Precision vs ±0.01@Ø250→looser". The Ø10 anchor
/// > holds (±0.01@Ø10 ⇒ IT6 ⇒ Precision). The "Ø250→looser" half is
/// > **physically backwards**: by ISO 286 a *fixed* ± is a *tighter* grade on
/// > a *larger* nominal (grade values scale up with size), so ±0.01@Ø250
/// > derives to ≤IT5 ⇒ [`ToleranceRange::UltraPrecision`] (tighter), not
/// > looser. Implemented per the ISO 286 physics the ADR mandates
/// > ("professionally-correct size-aware derivation") and the
/// > no-silent-under-quote posture (Q5); the resolved band is
/// > operator-overridable per job/feature. **Zero price impact in T2** (the
/// > band is not yet costed).
pub fn tightness(spec: ToleranceSpec, nominal_mm: f64) -> ToleranceRange {
    match spec {
        ToleranceSpec::Unspecified => ToleranceRange::Standard,
        ToleranceSpec::GeneralClass { class } => general_class_to_band(class),
        ToleranceSpec::ItGrade { grade } => it_grade_to_band(grade),
        ToleranceSpec::PlusMinus { value_mm } => {
            it_grade_to_band(derive_it_grade_from_plus_minus(value_mm, nominal_mm))
        }
        ToleranceSpec::PerDrawing => ToleranceRange::Standard,
    }
}

/// Normalise a [`ToleranceSpec`] into a [`NormalizedTolerance`] — the band
/// (via [`tightness`]), the manual-review flag (via
/// [`ToleranceSpec::requires_manual_review`]), and a deterministic
/// reasoning-log line. Pure; same inputs ⇒ byte-identical result (the log
/// line is the trust signal).
pub fn normalize_tolerance(spec: ToleranceSpec, nominal_mm: f64) -> NormalizedTolerance {
    let band = tightness(spec, nominal_mm);
    let manual_review = spec.requires_manual_review();
    let reason = match spec {
        ToleranceSpec::Unspecified => format!(
            "tolerance: unspecified -> defer to resolved target_tolerance (today's behaviour; band {})",
            band.as_db_str()
        ),
        ToleranceSpec::GeneralClass { class } => format!(
            "tolerance: ISO 2768 {} -> {} band",
            class.as_db_str(),
            band.as_db_str()
        ),
        ToleranceSpec::ItGrade { grade } => {
            format!("tolerance: IT{grade} -> {} band", band.as_db_str())
        }
        ToleranceSpec::PlusMinus { value_mm } => {
            let grade = derive_it_grade_from_plus_minus(value_mm, nominal_mm);
            format!(
                "tolerance: +/-{value_mm:.4}mm @ nominal {nominal_mm:.1}mm -> IT{grade} (ISO 286 size-aware) -> {} band",
                band.as_db_str()
            )
        }
        ToleranceSpec::PerDrawing => format!(
            "tolerance: per-drawing (GD&T) -> {} band + MANUAL REVIEW (not auto-priced; no silent tightening)",
            band.as_db_str()
        ),
    };
    NormalizedTolerance {
        band,
        manual_review,
        reason,
    }
}

/// Score a quote.
///
/// **Pure.** No I/O, no clock, no RNG, no allocation beyond the
/// returned struct + its `reasoning_log` strings. Same inputs ⇒
/// byte-identical output, every call.
///
/// See the crate-level docs for the input/output contract and the
/// 16-step algorithm summary.
pub fn quote(
    feature_graph: &FeatureGraph,
    materials: &[Material],
    complexity_rules: &[ComplexityRule],
    tolerance_multipliers: &[ToleranceMultiplier],
    stock_adjustments: &[StockAdjustment],
    parameters: &QuotingParameters,
    quantity: u32,
    target_tolerance: ToleranceRange,
) -> Result<QuoteBreakdown, QuoteError> {
    quote_with_calibration(
        feature_graph,
        materials,
        complexity_rules,
        tolerance_multipliers,
        stock_adjustments,
        parameters,
        quantity,
        target_tolerance,
        &crate::calibration::CalibrationTable::neutral(),
    )
}

/// S429 — [`quote`] with a closed-loop calibration table applied.
///
/// Identical to [`quote`] except the routed machine family's coefficient (from
/// `calibration`) scales the geometry-driven `machining_minutes` before it is
/// costed — and therefore the machining cost, subtotal, overhead, margin,
/// total, and lead-time projection all stay internally consistent. With a
/// neutral table the output is byte-identical to [`quote`] (no extra reasoning
/// line, coefficient `1.0`). Still pure.
#[allow(clippy::too_many_arguments)]
pub fn quote_with_calibration(
    feature_graph: &FeatureGraph,
    materials: &[Material],
    complexity_rules: &[ComplexityRule],
    tolerance_multipliers: &[ToleranceMultiplier],
    stock_adjustments: &[StockAdjustment],
    parameters: &QuotingParameters,
    quantity: u32,
    target_tolerance: ToleranceRange,
    calibration: &crate::calibration::CalibrationTable,
) -> Result<QuoteBreakdown, QuoteError> {
    // ADR-0094 Gap 2: delegate to the shop-model superset with an EMPTY
    // machine-rate slice ⇒ no family rate ⇒ the global flat rate is used ⇒
    // output (numbers + reasoning_log) is byte-identical to pre-ADR-0094.
    quote_with_shop_model(
        feature_graph,
        materials,
        complexity_rules,
        tolerance_multipliers,
        stock_adjustments,
        parameters,
        quantity,
        target_tolerance,
        calibration,
        &[],
        &[],
    )
}

/// S3 / ADR-0094 Gap 2 — the superset entry point. Identical to
/// [`quote_with_calibration`] except it additionally accepts the shop's
/// per-[`MachineFamily`] [`MachineRate`] snapshot. [`route_family`] picks the
/// part's machine family from its geometry (turned/round stock within
/// `bar_capacity_mm` ⇒ lights-out Swiss; larger round ⇒ turn-mill; prismatic
/// ⇒ 3-axis; the 5-axis flag wins outright), and the machining minutes are
/// costed at that family's effective EUR/min — `attended_rate ×
/// lights_out_factor` when the family is `unattended_capable` and the job
/// qualifies (turned bar stock at/above the setup-amortization quantity),
/// otherwise the attended rate.
///
/// **Inert by default:** an empty `machine_rates` slice, or no row for the
/// routed family, falls back to the global `machining_rate_eur_per_minute`
/// and emits TODAY'S EXACT machining line (no extra reasoning line) — so
/// every existing golden/determinism/branch/property number is unchanged.
/// Still pure.
#[allow(clippy::too_many_arguments)]
pub fn quote_with_shop_model(
    feature_graph: &FeatureGraph,
    materials: &[Material],
    complexity_rules: &[ComplexityRule],
    tolerance_multipliers: &[ToleranceMultiplier],
    stock_adjustments: &[StockAdjustment],
    parameters: &QuotingParameters,
    quantity: u32,
    target_tolerance: ToleranceRange,
    calibration: &crate::calibration::CalibrationTable,
    machine_rates: &[MachineRate],
    gear_process_rates: &[GearProcessRate],
) -> Result<QuoteBreakdown, QuoteError> {
    quote_with_catalogue(
        feature_graph,
        &CatalogueSnapshot {
            materials,
            complexity_rules,
            tolerance_multipliers,
            stock_adjustments,
            machine_rates,
            gear_process_rates,
            // ADR-0097 T3: legacy entry points pass an empty rate slice ⇒ the
            // additive tolerance-cost path is inert ⇒ byte-identical pricing.
            tolerance_cost_rates: &[],
        },
        parameters,
        quantity,
        target_tolerance,
        calibration,
    )
}

/// ADR-0094 Q2 / ADR-0097 T1 — the **catalogue-snapshot** superset entry
/// point, and the single home of the scoring algorithm. Behaviourally
/// identical to [`quote_with_shop_model`]: it just takes the six catalogue
/// slices as one borrowed [`CatalogueSnapshot`] instead of six positional
/// arguments, so the forthcoming `ToleranceCostRate` slice (ADR-0097 Part 2)
/// becomes a snapshot *field* rather than a 12th argument.
///
/// [`quote`], [`quote_with_calibration`] and [`quote_with_shop_model`] all
/// delegate here, so an empty/default snapshot reproduces today's numbers
/// and `reasoning_log` byte-for-byte. Still **pure** — no I/O, clock, RNG,
/// async or global state; same inputs ⇒ byte-identical output.
pub fn quote_with_catalogue(
    feature_graph: &FeatureGraph,
    catalogue: &CatalogueSnapshot,
    parameters: &QuotingParameters,
    quantity: u32,
    target_tolerance: ToleranceRange,
    calibration: &crate::calibration::CalibrationTable,
) -> Result<QuoteBreakdown, QuoteError> {
    // Re-bind the bundled catalogue slices to the exact local names the
    // algorithm body uses, so the scoring logic below is the unchanged
    // pre-ADR-0097 body (byte-identical numbers + reasoning_log).
    let CatalogueSnapshot {
        materials,
        complexity_rules,
        tolerance_multipliers,
        stock_adjustments,
        machine_rates,
        gear_process_rates,
        tolerance_cost_rates,
    } = *catalogue;
    // ── Pre-flight validation ─────────────────────────────────────
    if quantity == 0 {
        return Err(QuoteError::QuantityZero);
    }
    if feature_graph.schema_version > FeatureGraph::SCHEMA_VERSION {
        return Err(QuoteError::UnsupportedSchemaVersion {
            got: feature_graph.schema_version,
            supported: FeatureGraph::SCHEMA_VERSION,
        });
    }
    // Surface inverted-bound snapshot rows up front so a corrupt
    // catalogue is loud, not silently no-matching during the feature
    // loop below.
    for rule in complexity_rules {
        if let Some(max) = rule.count_max {
            if rule.count_min > max {
                return Err(QuoteError::InvalidComplexityRule {
                    rule_id: rule.id,
                    count_min: rule.count_min,
                    count_max: rule.count_max,
                });
            }
        }
    }

    let material = materials
        .iter()
        .find(|m| m.grade == feature_graph.material_grade)
        .ok_or_else(|| QuoteError::MaterialNotInCatalogue {
            grade: feature_graph.material_grade.clone(),
        })?;

    let tolerance = tolerance_multipliers
        .iter()
        .find(|t| t.tolerance_range == target_tolerance.as_db_str())
        .ok_or_else(|| QuoteError::ToleranceNotInTable {
            tolerance: target_tolerance.as_db_str().to_string(),
        })?;

    let mut log: Vec<String> = Vec::with_capacity(32);
    log.push(format!(
        "[engine v{ver}] inputs: grade={grade}, qty={qty}, tolerance={tol}, schema_v{schema}",
        ver = ENGINE_VERSION,
        grade = material.grade,
        qty = quantity,
        tol = target_tolerance.as_db_str(),
        schema = feature_graph.schema_version,
    ));

    // ── Step 1–2: stock block + base material cost (report §6.4) ──
    // A CNC shop buys a block sized to the bounding box (+ oversize) and
    // cuts most of it to chips, so material is billed on the STOCK
    // volume, not the finished-part volume. `scrap_factor` is the
    // stock-oversize margin. The same `stock_volume` drives roughing
    // removal below (one stock definition, report §5.1).
    let [bx, by, bz] = feature_graph.bounding_box_mm;
    // `bbox_volume` stays the bounding-box block: it still drives the
    // CAD-CAM `fill_ratio` (step 9) and the bbox-area finishing fallback,
    // which are geometry signals independent of the stock form.
    let bbox_volume = bx * by * bz;
    // ── ADR-0094 Gap 1: stock FORM sets the bought/roughed volume ──
    // RectangularBlock == bbox_volume (today's math, byte-for-byte).
    // RoundBar/Tube evaluate their own closed-form volume; the engine
    // never infers a spin axis — the form carries its own dimensions
    // (the extractor classifies; the engine evaluates).
    let form_volume = match feature_graph.stock_form {
        StockForm::RectangularBlock => bbox_volume,
        StockForm::RoundBar {
            diameter_mm,
            length_mm,
        } => std::f64::consts::FRAC_PI_4 * diameter_mm * diameter_mm * length_mm,
        StockForm::Tube {
            od_mm,
            id_mm,
            length_mm,
        } => std::f64::consts::FRAC_PI_4 * (od_mm * od_mm - id_mm * id_mm) * length_mm,
    };
    let stock_volume = form_volume * (1.0 + parameters.scrap_factor);
    match feature_graph.stock_form {
        // RectangularBlock emits TODAY'S EXACT line → golden byte-identical.
        StockForm::RectangularBlock => log.push(format!(
            "[material] bbox {bx:.3}×{by:.3}×{bz:.3} = bbox_volume_mm3={bv:.4} * (1 + scrap_factor={sc:.4}) = stock_volume_mm3={sv:.4}",
            bv = bbox_volume,
            sc = parameters.scrap_factor,
            sv = stock_volume,
        )),
        StockForm::RoundBar {
            diameter_mm,
            length_mm,
        } => {
            log.push(format!(
                "[material] stock_form=round_bar: π/4 * d={d:.3}² * L={l:.3} = form_volume_mm3={fv:.4} (vs bbox_volume_mm3={bv:.4})",
                d = diameter_mm,
                l = length_mm,
                fv = form_volume,
                bv = bbox_volume,
            ));
            log.push(format!(
                "[material] stock_volume_mm3 = form_volume {fv:.4} * (1 + scrap_factor={sc:.4}) = {sv:.4}",
                fv = form_volume,
                sc = parameters.scrap_factor,
                sv = stock_volume,
            ));
        }
        StockForm::Tube {
            od_mm,
            id_mm,
            length_mm,
        } => {
            log.push(format!(
                "[material] stock_form=tube: π/4 * (od={od:.3}² - id={id:.3}²) * L={l:.3} = form_volume_mm3={fv:.4} (bore not bought; vs bbox_volume_mm3={bv:.4})",
                od = od_mm,
                id = id_mm,
                l = length_mm,
                fv = form_volume,
                bv = bbox_volume,
            ));
            log.push(format!(
                "[material] stock_volume_mm3 = form_volume {fv:.4} * (1 + scrap_factor={sc:.4}) = {sv:.4}",
                fv = form_volume,
                sc = parameters.scrap_factor,
                sv = stock_volume,
            ));
        }
    }
    // mass_kg = stock_volume_mm3 × (g/cm3) × 1e-6   (mm3→cm3: /1000, g→kg: /1000)
    let mass_kg = stock_volume * material.density_g_cm3 / 1_000_000.0;
    let mut material_cost = mass_kg * material.cost_per_kg_eur;
    log.push(format!(
        "[material] mass_kg=stock_volume * density_g_cm3={d:.4} / 1e6 = {m:.6}; * cost_per_kg_eur={cpk:.4} = base_material_cost={mc:.4} EUR",
        d = material.density_g_cm3,
        m = mass_kg,
        cpk = material.cost_per_kg_eur,
        mc = material_cost,
    ));

    // ── Step 10: stock adjustment (multiplicative) ────────────────
    if let Some(adj) = stock_adjustments
        .iter()
        .find(|a| a.grade == material.grade && a.stock_status == material.stock_status.as_db_str())
    {
        let before = material_cost;
        material_cost *= 1.0 + adj.price_adjustment_pct;
        log.push(format!(
            "[material] stock_adjustment[{g}/{ss}]={pct:+.4} → material_cost: {b:.4} → {a:.4} EUR",
            g = adj.grade,
            ss = adj.stock_status,
            pct = adj.price_adjustment_pct,
            b = before,
            a = material_cost,
        ));
    } else {
        log.push(format!(
            "[material] no stock_adjustment for [{g}/{ss}] — no change",
            g = material.grade,
            ss = material.stock_status.as_db_str(),
        ));
    }

    // ── Step 11: exotic-material tax ──────────────────────────────
    if is_exotic_material(&material.grade) {
        let before = material_cost;
        material_cost *= 1.0 + parameters.exotic_material_tax;
        log.push(format!(
            "[material] exotic-material tax: grade `{g}` matches exotic set; * (1 + {tax:.4}) → {b:.4} → {a:.4} EUR",
            g = material.grade,
            tax = parameters.exotic_material_tax,
            b = before,
            a = material_cost,
        ));
    } else {
        log.push(format!(
            "[material] grade `{g}` not exotic — no tax",
            g = material.grade
        ));
    }

    // ── Step 3–4: complexity rules → machining_minutes + setup ────
    // Track (rule_id → setup_penalty_minutes) to add each unique
    // rule's penalty exactly once. BTreeMap (not HashMap) for
    // deterministic iteration in the log line below.
    let mut fired_setup_penalties: std::collections::BTreeMap<i64, f64> =
        std::collections::BTreeMap::new();
    // Feature-graph machining time. 0 today: STL is a triangle soup with
    // no topology, and STEP v1 emits an empty `features[]` (report §3).
    // Kept additive so a future feature-mining cut layers rule time on
    // top of the geometry model below without a re-wire.
    let mut feature_machining_minutes: f64 = 0.0;

    for (idx, feature) in feature_graph.features.iter().enumerate() {
        let bucket = SizeBucket::bucket(feature.representative_size_mm);
        let rule = pick_complexity_rule(
            complexity_rules,
            feature.feature_type.as_db_str(),
            bucket.as_db_str(),
            feature.count,
        )
        .ok_or_else(|| QuoteError::NoComplexityRuleForFeature {
            feature_type: feature.feature_type.as_db_str().to_string(),
            size_bucket: bucket.as_db_str().to_string(),
            count: feature.count,
        })?;

        let time_for_feature = rule.base_time_minutes * (feature.count as f64) * rule.multiplier;
        feature_machining_minutes += time_for_feature;
        log.push(format!(
            "[feature {i}] {ft}/{sb}/count={c} (size={sz:.3}mm) → rule#{rid} base={base:.3}min * count={c} * mult={mul:.3} = {t:.4} min",
            i = idx,
            ft = feature.feature_type.as_db_str(),
            sb = bucket.as_db_str(),
            c = feature.count,
            sz = feature.representative_size_mm,
            rid = rule.id,
            base = rule.base_time_minutes,
            mul = rule.multiplier,
            t = time_for_feature,
        ));
        fired_setup_penalties.insert(rule.id, rule.setup_penalty_minutes);
    }

    let total_setup_penalty: f64 = fired_setup_penalties.values().copied().sum();
    log.push(format!(
        "[setup] unique rules fired: {n}; total setup_penalty_minutes={tsp:.4}",
        n = fired_setup_penalties.len(),
        tsp = total_setup_penalty,
    ));
    // ── Step 3: geometry-driven machining time (report §5) ────────
    // Roughing: bulk removal, volume-driven. Finishing: surface
    // passes, area-driven. `machining_difficulty` MULTIPLIES both
    // (6061-T6 1.0 ⇒ fast, Inconel 5.0 ⇒ 5× slower). The pre-S418
    // `machinability_index` DIVISOR is deleted: its seed was inverted
    // and would have priced Inconel as the cheapest metal (report §6.1).
    let removed_volume_cm3 = (stock_volume - feature_graph.volume_mm3).max(0.0) / 1000.0;
    let mrr_ref = if parameters.mrr_rough_ref_cm3_per_min > 0.0 {
        parameters.mrr_rough_ref_cm3_per_min
    } else {
        MRR_ROUGH_REF_FALLBACK
    };
    let roughing_min = removed_volume_cm3 * material.machining_difficulty / mrr_ref;
    log.push(format!(
        "[machining] removed_volume_cm3 = (stock {sv:.4} - part {pv:.4})/1000 max 0 = {rv:.4}; roughing_min = {rv:.4} * difficulty={diff:.4} / MRR_ref={mrr:.4} = {rm:.4} min",
        sv = stock_volume,
        pv = feature_graph.volume_mm3,
        rv = removed_volume_cm3,
        diff = material.machining_difficulty,
        mrr = mrr_ref,
        rm = roughing_min,
    ));

    // Surface area: real value from the v2 extractor; fall back to the
    // bounding-box surface area 2(xy+yz+zx) on a v1/corrupt graph
    // (report §5.4) — a monotone floor, never zero finishing time.
    let surface_area_cm2 = if feature_graph.surface_area_mm2 > 0.0 {
        feature_graph.surface_area_mm2 / 100.0
    } else {
        let bbox_area = 2.0 * (bx * by + by * bz + bx * bz);
        log.push(format!(
            "[machining] surface_area_mm2 absent/≤0 → bbox-area fallback 2*({bx:.3}*{by:.3}+{by:.3}*{bz:.3}+{bx:.3}*{bz:.3}) = {a:.4} mm²",
            a = bbox_area,
        ));
        bbox_area / 100.0
    };
    let finishing_min =
        surface_area_cm2 * parameters.t_finish_min_per_cm2 * material.machining_difficulty;
    log.push(format!(
        "[machining] finishing_min = surface_area_cm2={a:.4} * t_finish={tf:.4} * difficulty={diff:.4} = {fm:.4} min",
        a = surface_area_cm2,
        tf = parameters.t_finish_min_per_cm2,
        diff = material.machining_difficulty,
        fm = finishing_min,
    ));

    let machining_minutes_base = roughing_min + finishing_min + feature_machining_minutes;
    log.push(format!(
        "[machining] machining_minutes = roughing {rm:.4} + finishing {fm:.4} + feature {fmm:.4} = {mm:.4} min",
        rm = roughing_min,
        fm = finishing_min,
        fmm = feature_machining_minutes,
        mm = machining_minutes_base,
    ));

    // ── S429: closed-loop calibration ─────────────────────────────
    // Scale the geometry estimate by the routed family's learned
    // coefficient (mean actual/estimated from past jobs). Applied here —
    // before cost — so machining_cost, subtotal, overhead, margin, total
    // and the lead-time projection all stay consistent. Neutral (1.0)
    // tables add no line and leave the value untouched: pre-calibration
    // pricing is byte-identical.
    // ── ADR-0094 Gap 2: route the part to a machine family ────────
    // Geometry-driven; supersedes `MachineFamily::for_route` (kept for other
    // callers). For the default `RectangularBlock` form this returns exactly
    // `for_route(requires_5_axis)` (ThreeAxisMill, or FiveAxisMill with the
    // 5-axis flag), so every existing golden is byte-identical. The routed
    // family keys BOTH the S429 calibration coefficient and the Gap-2 machine
    // rate — one enum, three uses (capacity, calibration, rate).
    let routed_family = route_family(
        feature_graph.stock_form,
        feature_graph.requires_5_axis,
        stock_od_mm(feature_graph.stock_form),
        parameters,
    );
    let calibration_family = routed_family;
    let calibration_coefficient = calibration.coefficient(calibration_family);
    let machining_minutes = if (calibration_coefficient - 1.0).abs() > f64::EPSILON {
        let adjusted = machining_minutes_base * calibration_coefficient;
        log.push(format!(
            "[calibration] family={fam} coefficient={c:.4}x (set {hash}): machining_minutes {base:.4} -> {adj:.4} min",
            fam = calibration_family.as_db_str(),
            c = calibration_coefficient,
            hash = calibration.set_hash(),
            base = machining_minutes_base,
            adj = adjusted,
        ));
        adjusted
    } else {
        machining_minutes_base
    };

    // ── Step 4: inspection_minutes ────────────────────────────────
    // Per-feature-row count (NOT sum of `feature.count`) — one
    // inspection setup per drawing callout, not per hole.
    let feature_row_count = feature_graph.features.len() as f64;
    let inspection_minutes = tolerance.inspection_minutes_per_feature * feature_row_count;
    log.push(format!(
        "[inspection] tolerance.inspection_minutes_per_feature={ipf:.4} * feature_rows={frc} = inspection_minutes={im:.4}",
        ipf = tolerance.inspection_minutes_per_feature,
        frc = feature_row_count,
        im = inspection_minutes,
    ));

    // ── Step 5: machining cost (report §5.3) ──────────────────────
    // ADR-0094 Gap 2: effective machine-family rate. Default = today's
    // global flat rate. A matching `MachineRate` row switches to the routed
    // family's effective EUR/min: the attended rate, discounted by
    // `lights_out_factor` when the family is `unattended_capable` AND the job
    // qualifies (turned/round bar stock at/above the setup-amortization
    // quantity — one operator tends several spindles overnight, so cost-per-
    // minute drops while the physical cut minutes are unchanged). Empty slice
    // / no matching row ⇒ stays the global rate and adds NO line ⇒ pricing is
    // byte-identical to pre-ADR-0094. (Setup cost stays on the global rate —
    // setup is attended; see ADR-0094 Gap 2 + the S3 hand-off note.)
    let mut machining_rate = parameters.machining_rate_eur_per_minute;
    if let Some(rate) = machine_rates
        .iter()
        .find(|r| r.family == routed_family.as_db_str())
    {
        let lights_out_eligible = rate.unattended_capable
            && is_turned_bar_stock(feature_graph.stock_form)
            && quantity >= parameters.setup_amortization_threshold;
        machining_rate = if lights_out_eligible {
            rate.attended_rate_eur_per_min * rate.lights_out_factor
        } else {
            rate.attended_rate_eur_per_min
        };
        log.push(format!(
            "[machining] routed_family={fam}: machine-rate row matched (attended={att:.4} EUR/min, lights_out_factor={lof:.4}, unattended_capable={uc}); lights_out_eligible={loe} → effective_rate={eff:.4} EUR/min (global was {glob:.4})",
            fam = routed_family.as_db_str(),
            att = rate.attended_rate_eur_per_min,
            lof = rate.lights_out_factor,
            uc = rate.unattended_capable,
            loe = lights_out_eligible,
            eff = machining_rate,
            glob = parameters.machining_rate_eur_per_minute,
        ));
    }

    let billable_minutes = machining_minutes + inspection_minutes;
    let mut machining_cost = billable_minutes * machining_rate * tolerance.multiplier;
    log.push(format!(
        "[machining] (machining_minutes={mm:.4} + inspection_minutes={im:.4}) = billable={bm:.4} min; * rate={r:.4} EUR/min * tolerance_mult={tmu:.4} = machining_cost={mc:.4} EUR",
        mm = machining_minutes,
        im = inspection_minutes,
        bm = billable_minutes,
        r = machining_rate,
        tmu = tolerance.multiplier,
        mc = machining_cost,
    ));

    // ── Step 6: thin-wall + tight-tolerance bump ──────────────────
    if feature_graph.thin_wall_present && target_tolerance >= ToleranceRange::Tight {
        let before = machining_cost;
        machining_cost *= THIN_WALL_TIGHT_TOL_BUMP;
        log.push(format!(
            "[machining] thin_wall_present && tolerance>=Tight → * THIN_WALL_TIGHT_TOL_BUMP={b:.4}: {bef:.4} → {aft:.4} EUR",
            b = THIN_WALL_TIGHT_TOL_BUMP,
            bef = before,
            aft = machining_cost,
        ));
    } else {
        log.push(format!(
            "[machining] thin_wall_present={tw} && tolerance>=Tight={tt} — no bump",
            tw = feature_graph.thin_wall_present,
            tt = target_tolerance >= ToleranceRange::Tight,
        ));
    }

    // ── Step 7: 5-axis routing flag ───────────────────────────────
    // Drives the setup-minutes 5-axis adder (step 9) + the CAD-CAM
    // complexity premium (step 10). No separate machine-rate split
    // day-1 (report §8.1).
    let route_to_5_axis = feature_graph.requires_5_axis;
    log.push(format!(
        "[routing] route_to_5_axis={r5} (drives setup 5-axis adder + CAD-CAM premium; no day-1 machine-rate split)",
        r5 = route_to_5_axis,
    ));

    // Operator-knob: per-material `quote_multiplier` is a final
    // machining-side override. Folded in here so the SPA's per-material
    // override knob has a visible effect without polluting margin %.
    if (material.quote_multiplier - 1.0).abs() > f64::EPSILON {
        let before = machining_cost;
        machining_cost *= material.quote_multiplier;
        log.push(format!(
            "[machining] material.quote_multiplier={qm:.4}: {b:.4} → {a:.4} EUR",
            qm = material.quote_multiplier,
            b = before,
            a = machining_cost,
        ));
    }

    // ── Step 8: setup cost (report §5.5) ──────────────────────────
    // Fixed base + a 5-axis adder + any fired-rule setup penalties
    // (0 today, no features), then amortised over qty at/above the
    // threshold.
    let setup_minutes = parameters.setup_base_min
        + if route_to_5_axis {
            parameters.setup_5axis_min
        } else {
            0.0
        }
        + total_setup_penalty;
    log.push(format!(
        "[setup] setup_minutes = base={base:.4} + 5axis={fivx:.4} + rule_penalty={tsp:.4} = {sm:.4} min",
        base = parameters.setup_base_min,
        fivx = if route_to_5_axis {
            parameters.setup_5axis_min
        } else {
            0.0
        },
        tsp = total_setup_penalty,
        sm = setup_minutes,
    ));
    let setup_cost = if quantity >= parameters.setup_amortization_threshold {
        let v = setup_minutes * parameters.machining_rate_eur_per_minute / (quantity as f64);
        log.push(format!(
            "[setup] qty={q} >= threshold={th} → setup_cost = setup_minutes={sm:.4} min * rate={r:.4} / qty = {v:.4} EUR/part",
            q = quantity,
            th = parameters.setup_amortization_threshold,
            sm = setup_minutes,
            r = parameters.machining_rate_eur_per_minute,
            v = v,
        ));
        v
    } else {
        let v = setup_minutes * parameters.machining_rate_eur_per_minute;
        log.push(format!(
            "[setup] qty={q} < threshold={th} → setup_cost = setup_minutes={sm:.4} min * rate={r:.4} = {v:.4} EUR/part (unamortised)",
            q = quantity,
            th = parameters.setup_amortization_threshold,
            sm = setup_minutes,
            r = parameters.machining_rate_eur_per_minute,
            v = v,
        ));
        v
    };

    // ── Step 9: CAD-CAM design cost (report §4) ───────────────────
    // One-time programming / fixturing, auto-derived from geometry
    // signals, amortised across the whole batch. clamp(base, base+Σ,
    // MAX): the operator-tunable base IS the effective floor (so
    // lowering it lowers one-off quotes, report §4.2), MAX caps it.
    let fill_ratio = if bbox_volume > 0.0 {
        feature_graph.volume_mm3 / bbox_volume
    } else {
        1.0
    };
    let max_bbox = bx.max(by).max(bz);
    let hard_material = material.machining_difficulty >= HARD_MATERIAL_DIFFICULTY_THRESHOLD;
    let mut cad_cam_hours = parameters.cad_cam_base_hours;
    let mut cam_signals: Vec<String> = vec![format!("base={:.2}", parameters.cad_cam_base_hours)];
    if route_to_5_axis {
        cad_cam_hours += CAD_CAM_5AXIS_HOURS;
        cam_signals.push(format!("5axis+{CAD_CAM_5AXIS_HOURS:.2}"));
    }
    if fill_ratio < LOW_FILL_RATIO {
        cad_cam_hours += CAD_CAM_LOW_FILL_HOURS;
        cam_signals.push(format!(
            "low_fill(<{LOW_FILL_RATIO:.2})+{CAD_CAM_LOW_FILL_HOURS:.2}"
        ));
    } else if fill_ratio < MED_FILL_RATIO {
        cad_cam_hours += CAD_CAM_MED_FILL_HOURS;
        cam_signals.push(format!(
            "med_fill(<{MED_FILL_RATIO:.2})+{CAD_CAM_MED_FILL_HOURS:.2}"
        ));
    }
    if feature_graph.thin_wall_present {
        cad_cam_hours += CAD_CAM_THIN_WALL_HOURS;
        cam_signals.push(format!("thin_wall+{CAD_CAM_THIN_WALL_HOURS:.2}"));
    }
    if max_bbox >= LARGE_ENVELOPE_MM {
        cad_cam_hours += CAD_CAM_LARGE_ENVELOPE_HOURS;
        cam_signals.push(format!(
            "large_env(>={LARGE_ENVELOPE_MM:.0}mm)+{CAD_CAM_LARGE_ENVELOPE_HOURS:.2}"
        ));
    }
    if hard_material {
        cad_cam_hours += CAD_CAM_HARD_MATERIAL_HOURS;
        cam_signals.push(format!(
            "hard_material(diff>={HARD_MATERIAL_DIFFICULTY_THRESHOLD:.1})+{CAD_CAM_HARD_MATERIAL_HOURS:.2}"
        ));
    }
    let cad_cam_hours_raw = cad_cam_hours;
    let cad_cam_hours = cad_cam_hours.clamp(0.0, CAD_CAM_MAX_HOURS);
    log.push(format!(
        "[cad_cam] fill_ratio={fr:.4}, max_bbox={mb:.3}mm, hard_material={hm}; hours = {sigs} = {raw:.4} → clamp(0,{max:.1}) = {h:.4} h",
        fr = fill_ratio,
        mb = max_bbox,
        hm = hard_material,
        sigs = cam_signals.join(" + "),
        raw = cad_cam_hours_raw,
        max = CAD_CAM_MAX_HOURS,
        h = cad_cam_hours,
    ));
    let cad_cam_cost = cad_cam_hours * parameters.cad_cam_rate_eur_per_hour / (quantity as f64);
    log.push(format!(
        "[cad_cam] cad_cam_cost = hours={h:.4} * rate={r:.4} EUR/h / qty={q} = {c:.4} EUR/part (amortised)",
        h = cad_cam_hours,
        r = parameters.cad_cam_rate_eur_per_hour,
        q = quantity,
        c = cad_cam_cost,
    ));

    // ── ADR-0094 Gap 3: gear-generation op cost ───────────────────
    // Per gear: resolve the process (Auto ⇒ `select_gear_process`), look up
    // its operator-tunable `GearProcessRate`, and compute
    //   gear_min = setup_min + z·min_per_tooth·module^exp·fw_factor·qual_factor
    // (× in_cycle_factor when power-skived in-cycle on a routed turning
    // family), costed at the routed family's EFFECTIVE €/min (the same rate
    // the machining line used). Summed into `gear_cost` and folded into the
    // subtotal. EMPTY gears ⇒ the loop never runs ⇒ `gear_cost` stays 0.0, NO
    // reasoning line is added, and the subtotal line below is TODAY'S EXACT
    // line ⇒ byte-identical pricing.
    let gear_cost = gear_op_cost(
        &feature_graph.gears,
        routed_family,
        machining_rate,
        gear_process_rates,
        &mut log,
    );

    // ── ADR-0097 Part 2 / T3: additive professional-tolerance cost ──
    // Itemised, costed at the routed EFFECTIVE EUR/min (the same rate the
    // machining line used; grinding at the Grinder family rate). EMPTY rate
    // slice ⇒ the path is never entered ⇒ `tolerance_cost` stays 0.0, NO
    // reasoning line is added, and the subtotal line below is TODAY'S EXACT
    // line ⇒ byte-identical pricing (mirrors the empty-`gears` posture above).
    let tolerance_cost = tolerance_op_cost(
        feature_graph,
        target_tolerance,
        material_cost,
        machining_cost,
        finishing_min,
        machining_rate,
        machine_rates,
        tolerance_cost_rates,
        &mut log,
    );

    // ── Step 10–13: subtotal → overhead → margin → total ─────────
    let subtotal =
        material_cost + machining_cost + setup_cost + cad_cam_cost + gear_cost + tolerance_cost;
    let overhead = subtotal * parameters.overhead_factor;
    let margin = (subtotal + overhead) * parameters.profit_margin_base;
    let total_price = subtotal + overhead + margin;
    // Byte-identity: when `tolerance_cost == 0.0` (empty/zero rate table or
    // default class) the pre-ADR-0097 gear-only branch is emitted VERBATIM; the
    // tolerance term is named only when non-zero (mirrors `gear_cost`).
    if crate::breakdown::is_zero_eur(&tolerance_cost) {
        if feature_graph.gears.is_empty() {
            log.push(format!(
                "[totals] material={m:.4} + machining={mc:.4} + setup={s:.4} + cad_cam={cc:.4} = subtotal={st:.4} EUR",
                m = material_cost,
                mc = machining_cost,
                s = setup_cost,
                cc = cad_cam_cost,
                st = subtotal,
            ));
        } else {
            log.push(format!(
                "[totals] material={m:.4} + machining={mc:.4} + setup={s:.4} + cad_cam={cc:.4} + gear={g:.4} = subtotal={st:.4} EUR",
                m = material_cost,
                mc = machining_cost,
                s = setup_cost,
                cc = cad_cam_cost,
                g = gear_cost,
                st = subtotal,
            ));
        }
    } else if feature_graph.gears.is_empty() {
        log.push(format!(
            "[totals] material={m:.4} + machining={mc:.4} + setup={s:.4} + cad_cam={cc:.4} + tolerance={t:.4} = subtotal={st:.4} EUR",
            m = material_cost,
            mc = machining_cost,
            s = setup_cost,
            cc = cad_cam_cost,
            t = tolerance_cost,
            st = subtotal,
        ));
    } else {
        log.push(format!(
            "[totals] material={m:.4} + machining={mc:.4} + setup={s:.4} + cad_cam={cc:.4} + gear={g:.4} + tolerance={t:.4} = subtotal={st:.4} EUR",
            m = material_cost,
            mc = machining_cost,
            s = setup_cost,
            cc = cad_cam_cost,
            g = gear_cost,
            t = tolerance_cost,
            st = subtotal,
        ));
    }
    log.push(format!(
        "[totals] overhead = subtotal * overhead_factor={of:.4} = {oh:.4} EUR",
        of = parameters.overhead_factor,
        oh = overhead,
    ));
    log.push(format!(
        "[totals] margin = (subtotal + overhead) * profit_margin_base={pmb:.4} = {mg:.4} EUR",
        pmb = parameters.profit_margin_base,
        mg = margin,
    ));
    log.push(format!(
        "[totals] total_price = subtotal + overhead + margin = {tp:.4} EUR",
        tp = total_price,
    ));

    // ── Step 16: min-margin floor check ───────────────────────────
    let actual_margin_pct = if total_price > 0.0 {
        margin / total_price
    } else {
        0.0
    };
    if actual_margin_pct < parameters.min_margin {
        return Err(QuoteError::MarginFloorViolation {
            actual_pct: actual_margin_pct,
            floor_pct: parameters.min_margin,
            total_price,
        });
    }
    log.push(format!(
        "[gate] margin/total = {amp:.4} >= min_margin floor {mm:.4} — OK",
        amp = actual_margin_pct,
        mm = parameters.min_margin,
    ));

    Ok(QuoteBreakdown {
        material_cost,
        machining_cost,
        cad_cam_cost,
        setup_cost,
        gear_cost,
        tolerance_cost,
        overhead,
        margin,
        total_price,
        machining_minutes,
        inspection_minutes,
        route_to_5_axis,
        calibration_coefficient,
        engine_version: ENGINE_VERSION.to_string(),
        reasoning_log: log,
    })
}

/// Pick the most-specific complexity rule for a feature triple.
///
/// **Precedence** (per the brief): bounded rules (`count_max =
/// Some(_)`) outrank unbounded; within bounded, the rule whose
/// `count_max - count_min` is smallest wins (tightest range). Within
/// unbounded, the largest `count_min` wins (most specific lower
/// bound for "rest of the tail"). Ties (same precedence key) are
/// broken by the rule with the lowest `id` for determinism.
///
/// Returns `None` if no rule matches the triple — caller maps to
/// [`crate::QuoteError::NoComplexityRuleForFeature`].
fn pick_complexity_rule<'a>(
    rules: &'a [ComplexityRule],
    feature_type: &str,
    size_bucket: &str,
    count: u32,
) -> Option<&'a ComplexityRule> {
    let mut best: Option<&ComplexityRule> = None;
    let mut best_key: Option<(u8, u64, i64)> = None;

    for rule in rules {
        if rule.feature_type != feature_type {
            continue;
        }
        if rule.size_bucket != size_bucket {
            continue;
        }
        if count < rule.count_min {
            continue;
        }
        if let Some(max) = rule.count_max {
            if count > max {
                continue;
            }
        }

        // Precedence key: (bounded_first, range_width, rule_id).
        // Bounded: tier=0, width=(max-min). Unbounded: tier=1,
        // width=(u32::MAX - count_min) so that a *larger* count_min
        // — i.e. more specific lower bound — sorts first.
        let (tier, width): (u8, u64) = match rule.count_max {
            Some(max) => (0, u64::from(max - rule.count_min)),
            None => (1, u64::from(u32::MAX - rule.count_min)),
        };
        let key = (tier, width, rule.id);
        if best_key.as_ref().is_none_or(|bk| key < *bk) {
            best = Some(rule);
            best_key = Some(key);
        }
    }
    best
}

/// S3 / ADR-0094 Gap 2 — pure geometry → machine-family routing.
///
/// Generalises [`MachineFamily::for_route`] (which only saw the 5-axis flag):
/// the 5-axis flag still wins outright; otherwise a round/tube blank routes
/// to the bar-fed, lights-out-capable [`MachineFamily::SwissTurnMill`] when
/// its outer diameter fits the bar feeder (`od_mm <= params.bar_capacity_mm`),
/// to [`MachineFamily::TurnMill`] when it is larger, and a prismatic
/// `RectangularBlock` routes to [`MachineFamily::ThreeAxisMill`] — exactly
/// what `for_route(false)` returned, so the default path is unchanged.
///
/// `od_mm` is supplied by the caller (derived from the stock form, or a
/// future extractor hint) and is consulted only for round/tube stock. Pure,
/// total, deterministic; the call site reasoning-logs the decision when it
/// affects the rate.
pub fn route_family(
    stock_form: StockForm,
    requires_5_axis: bool,
    od_mm: f64,
    params: &QuotingParameters,
) -> MachineFamily {
    if requires_5_axis {
        return MachineFamily::FiveAxisMill;
    }
    match stock_form {
        StockForm::RoundBar { .. } | StockForm::Tube { .. } => {
            if od_mm <= params.bar_capacity_mm {
                MachineFamily::SwissTurnMill
            } else {
                MachineFamily::TurnMill
            }
        }
        StockForm::RectangularBlock => MachineFamily::ThreeAxisMill,
    }
}

/// The outer diameter (mm) of a turned/round stock form, for
/// [`route_family`]'s bar-capacity test. `RectangularBlock` is prismatic — no
/// turning OD — so this returns `0.0` (unused: the prismatic branch ignores
/// `od_mm`).
fn stock_od_mm(stock_form: StockForm) -> f64 {
    match stock_form {
        StockForm::RoundBar { diameter_mm, .. } => diameter_mm,
        StockForm::Tube { od_mm, .. } => od_mm,
        StockForm::RectangularBlock => 0.0,
    }
}

/// Whether the stock form is turned/round bar stock — the lights-out
/// eligibility precondition (ADR-0094 Gap 2). A prismatic block is not.
fn is_turned_bar_stock(stock_form: StockForm) -> bool {
    matches!(
        stock_form,
        StockForm::RoundBar { .. } | StockForm::Tube { .. }
    )
}

/// S5 / ADR-0094 Gap 3 — deterministic gear-process selection for
/// [`GearProcess::Auto`]. Pure, total, reasoning-logged at the call site.
///
/// - **External** spur/helical ⇒ power-skiving when the part is routed to a
///   turning family ([`MachineFamily::SwissTurnMill`]/[`MachineFamily::TurnMill`])
///   — the teeth are generated in-cycle on the spindle that already holds the
///   part; otherwise hobbing on a dedicated hobber.
/// - **Internal** ring ⇒ gear shaping, escalating to wire-EDM STRICTLY ABOVE
///   [`GEAR_INTERNAL_WIRE_EDM_AGMA`] (the tightest classes need EDM's
///   tool-free precision). Broaching is never auto-selected — it is a
///   volume-driven operator override.
///
/// Public so the S6 SPA can preview the engine's choice (mirrors
/// [`route_family`] / [`is_exotic_material`]).
pub fn select_gear_process(
    kind: GearKind,
    routed_family: MachineFamily,
    quality_agma: u8,
) -> GearProcess {
    match kind {
        GearKind::ExternalSpurHelical => {
            if matches!(
                routed_family,
                MachineFamily::SwissTurnMill | MachineFamily::TurnMill
            ) {
                GearProcess::PowerSkive
            } else {
                GearProcess::Hob
            }
        }
        GearKind::InternalRing => {
            if quality_agma > GEAR_INTERNAL_WIRE_EDM_AGMA {
                GearProcess::WireEdm
            } else {
                GearProcess::Shape
            }
        }
    }
}

/// S5 / ADR-0094 Gap 3 — sum the per-gear tooth-generation op cost, reasoning-
/// logging each gear. Returns `0.0` for an EMPTY gear list WITHOUT touching
/// `log`, so the no-gear path is byte-identical to pre-Gap-3. A gear whose
/// resolved process has no [`GearProcessRate`] row contributes `0.0` and a
/// loud log line (fail-soft + surfaced per CLAUDE.md rule 12 — the trust log
/// is the signal). Pure, finite, non-negative for valid (non-negative) inputs.
fn gear_op_cost(
    gears: &[GearOp],
    routed_family: MachineFamily,
    effective_rate_eur_per_min: f64,
    gear_process_rates: &[GearProcessRate],
    log: &mut Vec<String>,
) -> f64 {
    let mut gear_cost = 0.0;
    for (gi, g) in gears.iter().enumerate() {
        // Resolve Auto → concrete process; reasoning-log the choice.
        let resolved = match g.process {
            GearProcess::Auto => {
                let sel = select_gear_process(g.kind, routed_family, g.quality_agma);
                log.push(format!(
                    "[gear {gi}] kind={k} process=auto → selected {sel} (routed_family={fam}, agma={q})",
                    k = g.kind.as_db_str(),
                    sel = sel.as_db_str(),
                    fam = routed_family.as_db_str(),
                    q = g.quality_agma,
                ));
                sel
            }
            forced => {
                log.push(format!(
                    "[gear {gi}] kind={k} process={p} (operator-forced)",
                    k = g.kind.as_db_str(),
                    p = forced.as_db_str(),
                ));
                forced
            }
        };
        // Look up the operator-tunable process coefficients.
        let Some(rate) = gear_process_rates
            .iter()
            .find(|r| r.process == resolved.as_db_str())
        else {
            log.push(format!(
                "[gear {gi}] WARNING no GearProcessRate row for process={p} → 0.0000 EUR (seed quoting_gear_processes)",
                p = resolved.as_db_str(),
            ));
            continue;
        };
        // gen_min = z · min_per_tooth · module^exp · facewidth_factor · quality_factor
        let quality_steps = g.quality_agma.saturating_sub(GEAR_AGMA_DATUM_CLASS);
        let quality_factor = 1.0 + (quality_steps as f64) * rate.agma_quality_factor_base;
        let facewidth_factor = (g.face_width_mm / GEAR_FACEWIDTH_REF_MM).max(0.0);
        // Guard a non-positive module (extractor/wiring validates positives;
        // defence-in-depth keeps `powf` finite, per the property test).
        let module_pow = if g.module_mm > 0.0 {
            g.module_mm.powf(rate.module_exponent)
        } else {
            0.0
        };
        let gen_min =
            (g.teeth as f64) * rate.min_per_tooth * module_pow * facewidth_factor * quality_factor;
        let base_gear_min = rate.setup_min + gen_min;
        log.push(format!(
            "[gear {gi}] setup={sm:.4} + z={z}·mpt={mpt:.4}·module^{me:.4}({mp:.4})·fw={fw:.4}·qual={qf:.4} = gear_min={gm:.4} min",
            sm = rate.setup_min,
            z = g.teeth,
            mpt = rate.min_per_tooth,
            me = rate.module_exponent,
            mp = module_pow,
            fw = facewidth_factor,
            qf = quality_factor,
            gm = base_gear_min,
        ));
        // In-cycle discount: power-skiving on the routed turning family.
        let in_cycle = resolved == GearProcess::PowerSkive
            && matches!(
                routed_family,
                MachineFamily::SwissTurnMill | MachineFamily::TurnMill
            );
        let gear_min = if in_cycle {
            let v = base_gear_min * rate.in_cycle_factor;
            log.push(format!(
                "[gear {gi}] in-cycle on {fam}: gear_min * in_cycle_factor={icf:.4} = {b:.4} → {a:.4} min",
                fam = routed_family.as_db_str(),
                icf = rate.in_cycle_factor,
                b = base_gear_min,
                a = v,
            ));
            v
        } else {
            base_gear_min
        };
        let this_cost = (gear_min * effective_rate_eur_per_min).max(0.0);
        gear_cost += this_cost;
        log.push(format!(
            "[gear {gi}] gear_min={gm:.4} min * effective_rate={r:.4} EUR/min = {c:.4} EUR",
            gm = gear_min,
            r = effective_rate_eur_per_min,
            c = this_cost,
        ));
    }
    if !gears.is_empty() {
        log.push(format!(
            "[gear] total gear_cost={gc:.4} EUR",
            gc = gear_cost
        ));
    }
    gear_cost
}

/// ADR-0097 Part 2 / T3 — the additive, itemised professional-tolerance cost.
///
/// Returns `0.0` for an EMPTY `tolerance_cost_rates` slice WITHOUT touching
/// `log`, so the no-rate-table path is byte-identical to pre-ADR-0097 (the
/// inert-by-default contract — mirrors [`gear_op_cost`]'s empty-`gears` posture).
///
/// With a rate table present it resolves the **governing band** — the tightest
/// of the resolved `target_tolerance` and every per-critical-feature callout
/// ([`FeatureGraph::critical_feature_tolerances`], normalised via
/// [`normalize_tolerance`]); a [`ToleranceSpec::PerDrawing`] callout raises a
/// loud manual-review line and is **never silently tightened** (its band is the
/// default — ADR-0097 Q5) — looks up that band's row, and sums four reasoning-
/// logged terms costed at the routed `effective_rate` (grinding at the
/// `Grinder` rate): (1) Σ critical features × (in-process gauging + CMM)
/// minutes; (2) extra slower-feed finishing passes; (3) a tightest-band
/// grinding adder (only when `grinding_escalation` AND the band is
/// [`ToleranceRange::UltraPrecision`]); (4) a scrap/rework uplift on
/// `(material_cost + machining_cost)`. A band with no matching row is fail-soft
/// `0.0` + a loud line (CLAUDE.md rule 12). Pure, finite, non-negative.
#[allow(clippy::too_many_arguments)]
fn tolerance_op_cost(
    feature_graph: &FeatureGraph,
    target_tolerance: ToleranceRange,
    material_cost: f64,
    machining_cost: f64,
    base_finish_min: f64,
    effective_rate_eur_per_min: f64,
    machine_rates: &[MachineRate],
    tolerance_cost_rates: &[ToleranceCostRate],
    log: &mut Vec<String>,
) -> f64 {
    // Inert guard: no rate table ⇒ never entered ⇒ 0.0, NO log line ⇒ today's
    // exact bytes (the golden's `tolerance = Standard`, empty-slice path).
    if tolerance_cost_rates.is_empty() {
        return 0.0;
    }

    // Resolve the governing band: the tightest of the resolved overall band and
    // every per-critical-feature callout. Per-drawing flags review; it never
    // silently tightens (Q5) — its band is the default.
    let mut governing = target_tolerance;
    let n_critical = feature_graph.critical_feature_tolerances.len();
    for ft in &feature_graph.critical_feature_tolerances {
        let nominal = feature_graph
            .features
            .get(ft.feature_index)
            .map(|f| f.representative_size_mm)
            .unwrap_or(0.0);
        let nt = normalize_tolerance(ft.spec, nominal);
        log.push(format!(
            "[tolerance] critical feature #{idx}: {reason}",
            idx = ft.feature_index,
            reason = nt.reason,
        ));
        if nt.manual_review {
            log.push(format!(
                "[tolerance] critical feature #{idx} -> MANUAL REVIEW (per-drawing GD&T; priced at default band, not silently tightened)",
                idx = ft.feature_index,
            ));
        }
        if nt.band > governing {
            governing = nt.band;
        }
    }

    // Look up the governing band's rate row; fail-soft + loud if absent.
    let Some(rate) = tolerance_cost_rates
        .iter()
        .find(|r| r.tolerance_class == governing.as_db_str())
    else {
        log.push(format!(
            "[tolerance] WARNING no ToleranceCostRate row for band={band} -> tolerance_cost 0.0000 EUR (seed quoting_tolerance_cost_rates)",
            band = governing.as_db_str(),
        ));
        return 0.0;
    };

    log.push(format!(
        "[tolerance] governing band={band} (resolved target={tgt}, {n} critical-feature callout(s)) -> rate row matched",
        band = governing.as_db_str(),
        tgt = target_tolerance.as_db_str(),
        n = n_critical,
    ));

    // Term 1 — per-critical-feature in-process gauging + CMM minutes.
    let per_feature_min = rate.inproc_inspection_min + rate.cmm_min_per_critical_feature;
    let inspection_min = per_feature_min * (n_critical as f64);
    let inspection_cost = inspection_min * effective_rate_eur_per_min;
    log.push(format!(
        "[tolerance] inspection = ({ip:.4} in-proc + {cmm:.4} CMM) min/feat * {n} feat = {im:.4} min * rate={r:.4} EUR/min = {c:.4} EUR",
        ip = rate.inproc_inspection_min,
        cmm = rate.cmm_min_per_critical_feature,
        n = n_critical,
        im = inspection_min,
        r = effective_rate_eur_per_min,
        c = inspection_cost,
    ));

    // Term 2 — extra slower-feed finishing passes (whole-part).
    let finish_min = rate.finish_passes_add * base_finish_min * rate.feed_slowdown_factor;
    let finish_cost = finish_min * effective_rate_eur_per_min;
    log.push(format!(
        "[tolerance] finishing = finish_passes_add={fpa:.4} * base_finish_min={bfm:.4} * feed_slowdown={fsf:.4} = {fm:.4} min * rate={r:.4} EUR/min = {c:.4} EUR",
        fpa = rate.finish_passes_add,
        bfm = base_finish_min,
        fsf = rate.feed_slowdown_factor,
        fm = finish_min,
        r = effective_rate_eur_per_min,
        c = finish_cost,
    ));

    // Term 3 — tightest-band grinding escalation (per critical feature).
    let is_tightest = governing == ToleranceRange::UltraPrecision;
    let grinding_cost = if rate.grinding_escalation && is_tightest && n_critical > 0 {
        let grinder_rate = machine_rates
            .iter()
            .find(|r| r.family == MachineFamily::Grinder.as_db_str())
            .map(|r| r.attended_rate_eur_per_min)
            .unwrap_or(effective_rate_eur_per_min);
        let grinding_min = GRINDING_ESCALATION_MIN_PER_CRITICAL_FEATURE * (n_critical as f64);
        let c = grinding_min * grinder_rate;
        log.push(format!(
            "[tolerance] grinding escalation (band={band}): {gm:.4} min ({per:.4}/feat * {n} feat) * grinder_rate={gr:.4} EUR/min = {c:.4} EUR",
            band = governing.as_db_str(),
            gm = grinding_min,
            per = GRINDING_ESCALATION_MIN_PER_CRITICAL_FEATURE,
            n = n_critical,
            gr = grinder_rate,
            c = c,
        ));
        c
    } else {
        0.0
    };

    // Term 4 — scrap / rework uplift on (material + machining).
    let scrap_cost = rate.rework_scrap_pct * (material_cost + machining_cost);
    log.push(format!(
        "[tolerance] scrap/rework = rework_scrap_pct={rsp:.4} * (material={m:.4} + machining={mc:.4}) = {c:.4} EUR",
        rsp = rate.rework_scrap_pct,
        m = material_cost,
        mc = machining_cost,
        c = scrap_cost,
    ));

    let tolerance_cost = (inspection_cost + finish_cost + grinding_cost + scrap_cost).max(0.0);
    log.push(format!(
        "[tolerance] total tolerance_cost={tc:.4} EUR",
        tc = tolerance_cost,
    ));
    tolerance_cost
}
