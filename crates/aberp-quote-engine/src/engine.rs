//! The scoring function — design doc §10's 16-step deterministic
//! algorithm.
//!
//! Every step appends one line to `reasoning_log`. Reading the log
//! top-to-bottom reconstructs the price exactly. There is no hidden
//! contribution.

use crate::breakdown::QuoteBreakdown;
use crate::catalogue::{
    ComplexityRule, Material, QuotingParameters, StockAdjustment, ToleranceMultiplier,
};
use crate::error::QuoteError;
use crate::feature_graph::{FeatureGraph, SizeBucket, ToleranceRange};
use crate::ENGINE_VERSION;

/// Labor multiplier applied when the part has a thin wall AND the
/// target tolerance is `Tight` or higher. Design doc §10 step 7.
/// Pinned as a constant here so the golden test catches any drift.
///
/// `TODO(S271+)`: when the wiring layer adds machining rate per
/// machine class, this constant migrates to the `quoting_parameters`
/// row alongside the rate.
pub const THIN_WALL_TIGHT_TOL_BUMP: f64 = 1.15;

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

    // ── Step 1–2: material volume + scrap + base material cost ────
    let scrap_volume = feature_graph.volume_mm3 * (1.0 + parameters.scrap_factor);
    log.push(format!(
        "[material] volume_mm3={vol:.4} * (1 + scrap_factor={sc:.4}) = scrap_volume_mm3={sv:.4}",
        vol = feature_graph.volume_mm3,
        sc = parameters.scrap_factor,
        sv = scrap_volume,
    ));
    // mass_kg = volume_mm3 × (g/cm3) × 1e-6     (mm3→cm3: /1000, g→kg: /1000)
    let mass_kg = scrap_volume * material.density_g_cm3 / 1_000_000.0;
    let mut material_cost = mass_kg * material.cost_per_kg_eur;
    log.push(format!(
        "[material] mass_kg=scrap_volume * density_g_cm3={d:.4} / 1e6 = {m:.6}; * cost_per_kg_eur={cpk:.4} = base_material_cost={mc:.4} EUR",
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
    let mut machining_minutes: f64 = 0.0;

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
        machining_minutes += time_for_feature;
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
    log.push(format!(
        "[machining] sum machining_minutes={mm:.4}",
        mm = machining_minutes
    ));

    // ── Step 5: inspection_minutes ────────────────────────────────
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

    // ── Step 6: labor cost ────────────────────────────────────────
    // machinability_index acts as a divisor: harder material (idx<1)
    // ⇒ more time, easier (idx>1) ⇒ less time. Guard divide-by-zero
    // by treating ≤0 as 1.0 (an operator who types 0 gets the
    // baseline rather than NaN; documented in catalogue.rs).
    let safe_mi = if material.machinability_index > 0.0 {
        material.machinability_index
    } else {
        1.0
    };
    let total_minutes = machining_minutes / safe_mi + inspection_minutes;
    let mut labor_cost =
        total_minutes * parameters.machining_rate_eur_per_minute * tolerance.multiplier;
    log.push(format!(
        "[labor] (machining_minutes={mm:.4} / machinability_index={mi:.4} + inspection_minutes={im:.4}) = total_minutes={tm:.4}; * rate={r:.4} EUR/min * tolerance_mult={tmu:.4} = labor_cost={lc:.4} EUR",
        mm = machining_minutes,
        mi = safe_mi,
        im = inspection_minutes,
        tm = total_minutes,
        r = parameters.machining_rate_eur_per_minute,
        tmu = tolerance.multiplier,
        lc = labor_cost,
    ));

    // ── Step 7: thin-wall + tight-tolerance bump ──────────────────
    if feature_graph.thin_wall_present && target_tolerance >= ToleranceRange::Tight {
        let before = labor_cost;
        labor_cost *= THIN_WALL_TIGHT_TOL_BUMP;
        log.push(format!(
            "[labor] thin_wall_present && tolerance>=Tight → * THIN_WALL_TIGHT_TOL_BUMP={b:.4}: {bef:.4} → {aft:.4} EUR",
            b = THIN_WALL_TIGHT_TOL_BUMP,
            bef = before,
            aft = labor_cost,
        ));
    } else {
        log.push(format!(
            "[labor] thin_wall_present={tw} && tolerance>=Tight={tt} — no bump",
            tw = feature_graph.thin_wall_present,
            tt = target_tolerance >= ToleranceRange::Tight,
        ));
    }

    // ── Step 8: 5-axis routing flag ───────────────────────────────
    let route_to_5_axis = feature_graph.requires_5_axis;
    log.push(format!(
        "[routing] route_to_5_axis={r5} (no v1 upcharge — flagged for S270+ machine-rate split)",
        r5 = route_to_5_axis,
    ));

    // Operator-knob: per-material `quote_multiplier` is a final
    // labor-side override. Folded in here so the SPA's per-material
    // override knob has a visible effect without polluting margin %.
    if (material.quote_multiplier - 1.0).abs() > f64::EPSILON {
        let before = labor_cost;
        labor_cost *= material.quote_multiplier;
        log.push(format!(
            "[labor] material.quote_multiplier={qm:.4}: {b:.4} → {a:.4} EUR",
            qm = material.quote_multiplier,
            b = before,
            a = labor_cost,
        ));
    }

    // ── Step 9: setup cost amortisation ───────────────────────────
    let setup_cost = if quantity >= parameters.setup_amortization_threshold {
        let v = total_setup_penalty * parameters.machining_rate_eur_per_minute / (quantity as f64);
        log.push(format!(
            "[setup] qty={q} >= threshold={th} → setup_cost = total_setup_penalty={tsp:.4} min * rate={r:.4} / qty = {v:.4} EUR/part",
            q = quantity,
            th = parameters.setup_amortization_threshold,
            tsp = total_setup_penalty,
            r = parameters.machining_rate_eur_per_minute,
            v = v,
        ));
        v
    } else {
        let v = total_setup_penalty * parameters.machining_rate_eur_per_minute;
        log.push(format!(
            "[setup] qty={q} < threshold={th} → setup_cost = total_setup_penalty={tsp:.4} min * rate={r:.4} = {v:.4} EUR/part (unamortised)",
            q = quantity,
            th = parameters.setup_amortization_threshold,
            tsp = total_setup_penalty,
            r = parameters.machining_rate_eur_per_minute,
            v = v,
        ));
        v
    };

    // ── Step 12–15: subtotal → overhead → margin → total ─────────
    let subtotal = material_cost + labor_cost + setup_cost;
    let overhead = subtotal * parameters.overhead_factor;
    let margin = (subtotal + overhead) * parameters.profit_margin_base;
    let total_price = subtotal + overhead + margin;
    log.push(format!(
        "[totals] material={m:.4} + labor={l:.4} + setup={s:.4} = subtotal={st:.4} EUR",
        m = material_cost,
        l = labor_cost,
        s = setup_cost,
        st = subtotal,
    ));
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
        labor_cost,
        setup_cost,
        overhead,
        margin,
        total_price,
        machining_minutes,
        inspection_minutes,
        route_to_5_axis,
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
