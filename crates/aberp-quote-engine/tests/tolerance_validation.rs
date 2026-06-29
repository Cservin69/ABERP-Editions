//! ADR-0097 **T7 — end-to-end tolerance VALIDATION golden** on real ADR-0094
//! planetary parts, priced through [`quote_with_catalogue`] with a seeded
//! [`ToleranceCostRate`] catalogue. Companion to `docs/quote-tolerance-cost-
//! driver-plan.md` (T7) and `docs/findings/0097-t7-tolerance-validation-
//! 2026-06-29.md` (seed-rate provenance + per-line decomposition).
//!
//! This is the integration proof that the taxonomy (T2), the additive
//! `tolerance_cost` model (T3) and the `CatalogueSnapshot` entry (T1) compose
//! into a realistic, fully-decomposable, **inert-by-default** tolerance line on
//! an actual drawing-like scenario — the Ø100 planetary gearbox from
//! `planetary_box_validation.rs`. The two parts the plan's T7 names:
//!
//!   * the **sun** with a Ø-bore **H6** critical fit (IT6 → Precision band), and
//!   * the **ring** with an **UltraPrecision ground face** (IT5 → tightest band,
//!     firing the grinding escalation).
//!
//! ## The five structural wins (plan T7) proved here
//!
//! 1. **Every tolerance term is present in the reasoning log and reconstructs
//!    the line** — each `[tolerance] …= X EUR` sub-term is parsed back out and
//!    summed to the `total tolerance_cost` line and to `breakdown.tolerance_cost`.
//! 2. **Inert proof** — the Standard / no-callout variant is **byte-identical**
//!    to the ADR-0094 pinned per-component number (`PIN_SUN` / `PIN_RING`), with
//!    `tolerance_cost == 0.0` and **no** `[tolerance]` log line.
//! 3. **`tolerance_cost > 0` only when a tighter spec / critical callout is
//!    supplied** — a no-callout, a not-tighter (IT11 → Standard) callout, and an
//!    empty rate table all price `0.0`; only a genuinely tighter callout against
//!    a seeded row moves the number.
//! 4. **Grinding escalation fires only at the tightest band** — present on the
//!    ring (UltraPrecision), and proven absent when the same `grinding_escalation`
//!    flag sits on a Precision row.
//! 5. **Totals are finite, > 0, and above the no-tolerance baseline by exactly
//!    the itemised sum** — `total_with == round4(baseline_total + tolerance_cost
//!    × (1+overhead)(1+margin))`.
//!
//! ## Hand-derived numbers (cross-checked to 4 dp; chain in each test comment)
//!
//! Tolerance terms are costed at the **routed effective €/min** (the same rate
//! the machining line used): the sun routes Swiss-turn-mill lights-out
//! (1.50 × 0.35 = **0.5250**), the ring turn-mill lights-out (1.60 × 0.45 =
//! **0.7200**); grinding uses the `Grinder` family rate (**3.0000**). The seed
//! cost-rate table is the illustrative, clean-arithmetic table documented in the
//! findings note — NOT production gospel (the boot seed is zero-contribution;
//! the operator tunes `quoting_tolerance_cost_rates`).

mod common;

use aberp_quote_engine::{
    quote_with_catalogue, CalibrationTable, CatalogueSnapshot, FeatureGraph, FeatureTolerance,
    GearKind, GearOp, GearProcess, GearProcessRate, GeneralClass, MachineRate, Material,
    QuoteBreakdown, QuotingParameters, StockForm, StockStatus, ToleranceCostRate, ToleranceRange,
    ToleranceSpec,
};
use common::{catchall_complexity_rules, default_tolerance_multipliers, no_stock_adjustments};

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

// ── ADR-0094 planetary catalogue (verbatim from planetary_box_validation.rs)
//    plus a Grinder machine-rate row (inert for base pricing; used only by the
//    UltraPrecision grinding escalation). ───────────────────────────────────
fn aluminium_6061() -> Material {
    Material {
        grade: "6061-T6".to_string(),
        density_g_cm3: 2.7,
        cost_per_kg_eur: 6.0,
        machining_difficulty: 1.0,
        quote_multiplier: 1.0,
        stock_status: StockStatus::InStock,
    }
}

fn params() -> QuotingParameters {
    QuotingParameters {
        scrap_factor: 0.15,
        profit_margin_base: 0.35,
        overhead_factor: 0.20,
        setup_amortization_threshold: 5,
        min_margin: 0.10,
        exotic_material_tax: 0.05,
        machining_rate_eur_per_minute: 1.6667,
        cad_cam_rate_eur_per_hour: 100.0,
        cad_cam_base_hours: 1.0,
        mrr_rough_ref_cm3_per_min: 8.0,
        t_finish_min_per_cm2: 0.08,
        setup_base_min: 20.0,
        setup_5axis_min: 25.0,
        bar_capacity_mm: 32.0,
    }
}

fn machine_rates() -> Vec<MachineRate> {
    let r = |fam: &str, att, lof, unat| MachineRate {
        family: fam.to_string(),
        attended_rate_eur_per_min: att,
        lights_out_factor: lof,
        unattended_capable: unat,
    };
    vec![
        r("3-axis-mill", 1.6667, 1.0, false),
        r("4-axis-mill", 1.90, 1.0, false),
        r("5-axis-mill", 2.50, 1.0, false),
        r("swiss-turn-mill", 1.50, 0.35, true),
        r("turn-mill", 1.60, 0.45, true),
        r("lathe", 1.50, 0.40, true),
        // T7: Grinder family rate — the tightest-band escalation target
        // (ADR-0097 Part 2). Inert for base pricing (no part routes to it).
        r("grinder", 3.0, 1.0, false),
    ]
}

fn gear_rates() -> Vec<GearProcessRate> {
    let r = |p: &str, setup, mpt, mexp, agma, icf| GearProcessRate {
        process: p.to_string(),
        setup_min: setup,
        min_per_tooth: mpt,
        module_exponent: mexp,
        agma_quality_factor_base: agma,
        in_cycle_factor: icf,
    };
    vec![
        r("hob", 20.0, 0.30, 1.0, 0.10, 1.0),
        r("power_skive", 8.0, 0.10, 1.0, 0.10, 0.5),
        r("shape", 30.0, 0.50, 1.0, 0.15, 1.0),
        r("broach", 60.0, 0.05, 1.0, 0.10, 1.0),
        r("wire_edm", 15.0, 2.00, 1.0, 0.20, 1.0),
    ]
}

fn ext_gear(module: f64, teeth: u32, face: f64, agma: u8) -> GearOp {
    GearOp {
        kind: GearKind::ExternalSpurHelical,
        module_mm: module,
        teeth,
        face_width_mm: face,
        quality_agma: agma,
        process: GearProcess::Auto,
    }
}
fn int_gear(module: f64, teeth: u32, face: f64, agma: u8) -> GearOp {
    GearOp {
        kind: GearKind::InternalRing,
        module_mm: module,
        teeth,
        face_width_mm: face,
        quality_agma: agma,
        process: GearProcess::Auto,
    }
}

/// The planetary **sun** — Ø22×20 round bar, bore 8 (OD ≤ bar cap 32 ⇒ Swiss
/// lights-out), one external Z18 gear. Standard/Unspecified tolerance, no
/// callouts — the exact ADR-0094 graph that pins to `PIN_SUN`.
fn sun_graph() -> FeatureGraph {
    FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        tolerance: ToleranceSpec::Unspecified,
        critical_feature_tolerances: Vec::new(),
        bounding_box_mm: [22.0, 22.0, 20.0],
        volume_mm3: 6_597.0,
        surface_area_mm2: 0.0,
        material_grade: "6061-T6".to_string(),
        features: Vec::new(),
        requires_5_axis: false,
        thin_wall_present: false,
        stock_form: StockForm::RoundBar {
            diameter_mm: 22.0,
            length_mm: 20.0,
        },
        gears: vec![ext_gear(2.0, 18, 18.0, 8)],
    }
}

/// The planetary **ring** — Ø100×20 round bar (OD > bar cap ⇒ turn-mill),
/// external Z50 + internal Z60 ring teeth. The ADR-0094 graph that pins to
/// `PIN_RING`.
fn ring_graph() -> FeatureGraph {
    FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        tolerance: ToleranceSpec::Unspecified,
        critical_feature_tolerances: Vec::new(),
        bounding_box_mm: [100.0, 100.0, 20.0],
        volume_mm3: 61_500.0,
        surface_area_mm2: 0.0,
        material_grade: "6061-T6".to_string(),
        features: Vec::new(),
        requires_5_axis: false,
        thin_wall_present: false,
        stock_form: StockForm::RoundBar {
            diameter_mm: 100.0,
            length_mm: 20.0,
        },
        gears: vec![ext_gear(2.0, 50, 18.0, 8), int_gear(2.0, 60, 18.0, 8)],
    }
}

fn rate(
    band: &str,
    finish_passes_add: f64,
    inproc: f64,
    cmm: f64,
    scrap: f64,
    feed: f64,
    grind: bool,
) -> ToleranceCostRate {
    ToleranceCostRate {
        tolerance_class: band.to_string(),
        finish_passes_add,
        inproc_inspection_min: inproc,
        cmm_min_per_critical_feature: cmm,
        rework_scrap_pct: scrap,
        feed_slowdown_factor: feed,
        grinding_escalation: grind,
    }
}

/// The seeded illustrative cost-rate table (one row per band). Chosen for clean
/// hand-arithmetic — provenance + per-line decomposition in
/// `docs/findings/0097-t7-tolerance-validation-2026-06-29.md`. The production
/// boot seed is zero-contribution (ADR-0097 Q6).
fn seeded_rates() -> Vec<ToleranceCostRate> {
    vec![
        rate("loose", 0.0, 0.0, 0.0, 0.0, 1.0, false),
        rate("standard", 0.0, 0.0, 0.0, 0.0, 1.0, false),
        // Tight: scrap uplift only.
        rate("tight", 0.0, 0.0, 0.0, 0.10, 1.0, false),
        // Precision (the sun's Ø-bore H6 case): finishing + inspection + scrap.
        rate("precision", 1.0, 2.0, 3.0, 0.05, 1.5, false),
        // UltraPrecision (the ring's ground face): all four terms + grinding.
        rate("ultra_precision", 2.0, 3.0, 6.0, 0.10, 2.0, true),
    ]
}

fn with_callout(mut fg: FeatureGraph, idx: usize, spec: ToleranceSpec) -> FeatureGraph {
    fg.critical_feature_tolerances = vec![FeatureTolerance {
        feature_index: idx,
        spec,
    }];
    fg
}

/// Price a planetary component through [`quote_with_catalogue`] with the given
/// per-job tolerance-cost-rate table (qty 100 boxes, resolved overall band =
/// Standard, neutral calibration), using the ADR-0094 planetary catalogue.
fn price(fg: &FeatureGraph, rates: Vec<ToleranceCostRate>) -> QuoteBreakdown {
    let materials = vec![aluminium_6061()];
    let complexity_rules = catchall_complexity_rules();
    let tolerance_multipliers = default_tolerance_multipliers();
    let stock_adjustments = no_stock_adjustments();
    let machine = machine_rates();
    let gears = gear_rates();
    let snap = CatalogueSnapshot {
        materials: &materials,
        complexity_rules: &complexity_rules,
        tolerance_multipliers: &tolerance_multipliers,
        stock_adjustments: &stock_adjustments,
        machine_rates: &machine,
        gear_process_rates: &gears,
        tolerance_cost_rates: &rates,
    };
    quote_with_catalogue(
        fg,
        &snap,
        &params(),
        100,
        ToleranceRange::Standard,
        &CalibrationTable::neutral(),
    )
    .expect("planetary tolerance-validation quote must succeed")
}

/// Parse the trailing `… = <number> EUR` value off a reasoning-log line.
fn trailing_eur(line: &str) -> f64 {
    let cut = line.rfind(" EUR").expect("term line must end in EUR");
    let pre = &line[..cut];
    let num: String = pre
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    num.parse()
        .unwrap_or_else(|_| panic!("no trailing EUR number in `{line}`"))
}

fn term(log: &[String], needle: &str) -> f64 {
    log.iter()
        .find(|l| l.contains(needle))
        .map(|l| trailing_eur(l))
        .unwrap_or_else(|| panic!("missing `{needle}` in reasoning log"))
}

// ── ADR-0094 pinned per-component goldens (planetary_box_validation.rs) ─────
const PIN_SUN: f64 = 10.6314;
const PIN_RING: f64 = 227.3354;
// Overhead/margin propagation factor: (1 + overhead 0.20) * (1 + margin 0.35).
const TOTAL_FACTOR: f64 = 1.62;

#[test]
fn win2_standard_variant_is_byte_identical_to_adr0094() {
    // The inert proof: the exact ADR-0094 graphs (Unspecified tolerance, no
    // callouts) priced through quote_with_catalogue with an EMPTY rate table
    // reproduce the pinned per-component totals to 4 dp, contribute
    // tolerance_cost == 0.0, and emit NO `[tolerance]` reasoning line.
    for (name, fg, pin) in [
        ("sun", sun_graph(), PIN_SUN),
        ("ring", ring_graph(), PIN_RING),
    ] {
        let b = price(&fg, vec![]);
        assert_eq!(round4(b.total_price), pin, "{name}: ADR-0094 total drifted");
        assert_eq!(b.tolerance_cost, 0.0, "{name}: inert ⇒ tolerance_cost 0");
        assert!(
            !b.reasoning_log.iter().any(|l| l.contains("[tolerance]")),
            "{name}: inert ⇒ no [tolerance] log line"
        );
        // The subtotal line is the no-tolerance (gear) format — today's bytes.
        assert!(b.reasoning_log.iter().any(|l| l.contains("+ gear=")
            && l.contains("= subtotal=")
            && !l.contains("tolerance")));
    }

    // Inert even WITH a fully-seeded table, as long as no callout is supplied
    // (governing stays Standard, whose seeded row is zero-contribution).
    for (name, fg, pin) in [
        ("sun", sun_graph(), PIN_SUN),
        ("ring", ring_graph(), PIN_RING),
    ] {
        let b = price(&fg, seeded_rates());
        assert_eq!(b.tolerance_cost, 0.0, "{name}: seeded+no-callout ⇒ 0");
        assert_eq!(
            round4(b.total_price),
            pin,
            "{name}: seeded+no-callout total"
        );
    }
}

#[test]
fn win1_sun_h6_bore_itemised_and_reconstructs_from_log() {
    // Sun, Ø-bore H6 critical fit (IT6 → Precision; governing tighter than the
    // Standard overall). Routed effective rate = Swiss lights-out 0.5250 €/min.
    //   inspection = (2.0 in-proc + 3.0 CMM) * 1 feat = 5.0 min * 0.5250 = 2.6250
    //   finishing  = 1.0 * base_finish_min 2.1824 * feed 1.5 = 3.2736 min
    //                * 0.5250 = 1.7186
    //   grinding   = 0 (Precision is not the tightest band)
    //   scrap      = 0.05 * (material 0.141637 + machining 1.286595) = 0.0714
    //   tolerance_cost = 2.6250 + 1.7186 + 0.0714 = 4.4151
    let base = price(&sun_graph(), vec![]);
    let b = price(
        &with_callout(sun_graph(), 0, ToleranceSpec::ItGrade { grade: 6 }),
        seeded_rates(),
    );
    let log = &b.reasoning_log;

    // Per-term pins (hand-derived above).
    assert_eq!(round4(b.tolerance_cost), 4.4151, "sun tolerance_cost");
    assert_eq!(round4(term(log, "[tolerance] inspection =")), 2.6250);
    assert_eq!(round4(term(log, "[tolerance] finishing =")), 1.7186);
    assert_eq!(round4(term(log, "[tolerance] scrap/rework =")), 0.0714);

    // (#1) every term present in the log and reconstructing the line: the
    // parsed sub-terms sum to both the `total tolerance_cost` line and the
    // breakdown field.
    let recon = term(log, "[tolerance] inspection =")
        + term(log, "[tolerance] finishing =")
        + term(log, "[tolerance] scrap/rework =");
    // Sub-terms are logged to 4 dp, so their sum matches the full-precision
    // line to within display rounding (≤ 4 terms × 0.5e-4 accumulation).
    assert!(
        (recon - b.tolerance_cost).abs() < 5e-4,
        "log sub-terms must reconstruct the line (recon {recon:.6} vs {:.6})",
        b.tolerance_cost
    );
    assert_eq!(
        round4(term(log, "[tolerance] total tolerance_cost=")),
        round4(b.tolerance_cost)
    );

    // Independent (non-log) hand-checks for the terms with no engine-internal
    // unknown: inspection from the rate inputs × routed rate; scrap from the
    // breakdown's own material+machining.
    assert_eq!(round4((2.0 + 3.0) * 1.0 * 0.5250), 2.6250);
    assert_eq!(round4(0.05 * (b.material_cost + b.machining_cost)), 0.0714);

    // No grinding term at the Precision band.
    assert!(!log.iter().any(|l| l.contains("grinding escalation")));

    // The legacy machining line is untouched (keys on target=Standard).
    assert_eq!(round4(b.machining_cost), round4(base.machining_cost));

    // Governing-band + IT-derivation lines are present (trust signal).
    assert!(log
        .iter()
        .any(|l| l.contains("critical feature #0: tolerance: IT6 -> precision band")));
    assert!(log.iter().any(|l| l.contains(
        "governing band=precision (resolved target=standard, 1 critical-feature callout(s))"
    )));
}

#[test]
fn win1_4_ring_ultraprecision_grinding_at_tightest_band() {
    // Ring, UltraPrecision ground face (IT5 → tightest band). Routed effective
    // rate = turn-mill lights-out 0.7200 €/min; grinder rate 3.0000 €/min.
    //   inspection = (3.0 + 6.0) * 1 feat = 9.0 min * 0.7200 = 6.4800
    //   finishing  = 2.0 * base_finish_min 22.4000 * feed 2.0 = 89.6 min
    //                * 0.7200 = 64.5120
    //   grinding   = 12.0 min/feat * 1 feat * grinder 3.0000 = 36.0000
    //   scrap      = 0.10 * (material 2.926394 + machining 26.850742) = 2.9777
    //   tolerance_cost = 6.4800 + 64.5120 + 36.0000 + 2.9777 = 109.9697
    let b = price(
        &with_callout(ring_graph(), 0, ToleranceSpec::ItGrade { grade: 5 }),
        seeded_rates(),
    );
    let log = &b.reasoning_log;

    assert_eq!(round4(b.tolerance_cost), 109.9697, "ring tolerance_cost");
    assert_eq!(round4(term(log, "[tolerance] inspection =")), 6.4800);
    assert_eq!(round4(term(log, "[tolerance] finishing =")), 64.5120);
    assert_eq!(
        round4(term(log, "[tolerance] grinding escalation")),
        36.0000
    );
    assert_eq!(round4(term(log, "[tolerance] scrap/rework =")), 2.9777);

    // (#1) all four terms reconstruct the line.
    let recon = term(log, "[tolerance] inspection =")
        + term(log, "[tolerance] finishing =")
        + term(log, "[tolerance] grinding escalation")
        + term(log, "[tolerance] scrap/rework =");
    // Sub-terms are logged to 4 dp, so their sum matches the full-precision
    // line to within display rounding (≤ 4 terms × 0.5e-4 accumulation).
    assert!(
        (recon - b.tolerance_cost).abs() < 5e-4,
        "log sub-terms must reconstruct the line (recon {recon:.6} vs {:.6})",
        b.tolerance_cost
    );
    assert_eq!(
        round4(term(log, "[tolerance] total tolerance_cost=")),
        round4(b.tolerance_cost)
    );

    // (#4) grinding fired, and at the tightest band only.
    assert!(log.iter().any(|l| l.contains(
        "grinding escalation (band=ultra_precision): 12.0000 min (12.0000/feat * 1 feat) * grinder_rate=3.0000 EUR/min = 36.0000 EUR"
    )));
    // Independent grinding check: 12.0 min/feat * 1 * 3.0 €/min = 36.0.
    assert_eq!(round4(12.0 * 1.0 * 3.0), 36.0000);

    assert!(log
        .iter()
        .any(|l| l.contains("critical feature #0: tolerance: IT5 -> ultra_precision band")));
}

#[test]
fn win4_grinding_does_not_fire_below_tightest_band() {
    // The SAME grinding_escalation flag on a Precision row (IT6 callout) must
    // NOT add a grinding term — escalation is tightest-band-only (ADR-0097).
    let rates = vec![
        rate("precision", 0.0, 0.0, 0.0, 0.0, 1.0, true),
        rate("ultra_precision", 0.0, 0.0, 0.0, 0.0, 1.0, true),
    ];
    let b = price(
        &with_callout(ring_graph(), 0, ToleranceSpec::ItGrade { grade: 6 }),
        rates,
    );
    assert_eq!(
        b.tolerance_cost, 0.0,
        "Precision row with grinding flag ⇒ no grinding term ⇒ 0"
    );
    assert!(!b
        .reasoning_log
        .iter()
        .any(|l| l.contains("grinding escalation")));
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("governing band=precision")));
}

#[test]
fn win3_tolerance_cost_only_when_a_tighter_spec_is_supplied() {
    // Truth table on the sun: the additive line is non-zero ONLY for a
    // genuinely-tighter callout against a seeded row.
    // (a) no callout + seeded rows ⇒ 0 (governing Standard, zero row).
    assert_eq!(price(&sun_graph(), seeded_rates()).tolerance_cost, 0.0);
    // (b) a NOT-tighter callout (IT11 → Standard) + seeded rows ⇒ 0.
    assert_eq!(
        price(
            &with_callout(sun_graph(), 0, ToleranceSpec::ItGrade { grade: 11 }),
            seeded_rates()
        )
        .tolerance_cost,
        0.0
    );
    // (c) the tighter H6 (IT6 → Precision) callout + EMPTY rates ⇒ 0 (inert).
    assert_eq!(
        price(
            &with_callout(sun_graph(), 0, ToleranceSpec::ItGrade { grade: 6 }),
            vec![]
        )
        .tolerance_cost,
        0.0
    );
    // (d) the tighter callout + seeded rows ⇒ > 0.
    assert!(
        price(
            &with_callout(sun_graph(), 0, ToleranceSpec::ItGrade { grade: 6 }),
            seeded_rates()
        )
        .tolerance_cost
            > 0.0
    );
}

#[test]
fn win5_totals_finite_positive_and_above_baseline_by_itemised_sum() {
    // For both worked parts: total_with == baseline_total + tolerance_cost
    // propagated through overhead (×1.20) and margin (×1.35) = ×1.62.
    for (name, fg, spec) in [
        ("sun", sun_graph(), ToleranceSpec::ItGrade { grade: 6 }),
        ("ring", ring_graph(), ToleranceSpec::ItGrade { grade: 5 }),
    ] {
        let base = price(&fg, vec![]);
        let withtol = price(&with_callout(fg.clone(), 0, spec), seeded_rates());
        assert!(withtol.total_price.is_finite() && withtol.total_price > 0.0);
        assert!(
            withtol.total_price > base.total_price,
            "{name}: tolerance must raise the total"
        );
        let expected = base.total_price + withtol.tolerance_cost * TOTAL_FACTOR;
        assert_eq!(
            round4(withtol.total_price),
            round4(expected),
            "{name}: total must exceed baseline by exactly the itemised sum × 1.62"
        );
        // The subtotal line now names the tolerance term (folded into subtotal).
        assert!(withtol.reasoning_log.iter().any(|l| l.contains(&format!(
            "+ tolerance={:.4} = subtotal=",
            withtol.tolerance_cost
        ))));
    }
}

#[test]
fn per_drawing_raises_manual_review_flag_with_zero_silent_cost() {
    // "Per drawing" (GD&T) on the ring resolves to the DEFAULT band (Standard)
    // and raises the manual-review flag — NEVER silently tightened to the large
    // Precision/UltraPrecision rows (ADR-0097 Q5).
    let b = price(
        &with_callout(ring_graph(), 0, ToleranceSpec::PerDrawing),
        seeded_rates(),
    );
    // Standard row is zero-contribution ⇒ zero silent cost.
    assert_eq!(b.tolerance_cost, 0.0, "per-drawing ⇒ zero silent cost");
    let log = &b.reasoning_log;
    assert!(log
        .iter()
        .any(|l| l.contains("MANUAL REVIEW (per-drawing GD&T")));
    assert!(log.iter().any(|l| l.contains("governing band=standard")));
    // Never silently tightened.
    assert!(!log
        .iter()
        .any(|l| l.contains("governing band=precision")
            || l.contains("governing band=ultra_precision")));

    // And with no rate table at all: still inert, still no tightening.
    let b0 = price(
        &with_callout(ring_graph(), 0, ToleranceSpec::PerDrawing),
        vec![],
    );
    assert_eq!(b0.tolerance_cost, 0.0);
}

#[test]
fn general_class_iso2768_medium_is_inert_default() {
    // ISO 2768-medium (the universal title-block default) → Standard band ⇒
    // zero additive cost against the seeded table: today's price, unchanged.
    let b = price(
        &with_callout(
            sun_graph(),
            0,
            ToleranceSpec::GeneralClass {
                class: GeneralClass::Iso2768Medium,
            },
        ),
        seeded_rates(),
    );
    assert_eq!(b.tolerance_cost, 0.0);
    assert_eq!(round4(b.total_price), PIN_SUN);
}

#[test]
fn dump_decomposition_for_report() {
    // Always-passing dump; run with `--nocapture` to read the per-line € used
    // to author the findings note + verify the pins.
    eprintln!("\n=== ADR-0097 T7 tolerance validation — per-line € decomposition ===");
    for (name, fg, spec) in [
        (
            "sun  (H6 bore, IT6→precision)",
            sun_graph(),
            ToleranceSpec::ItGrade { grade: 6 },
        ),
        (
            "ring (ground face, IT5→ultra)",
            ring_graph(),
            ToleranceSpec::ItGrade { grade: 5 },
        ),
    ] {
        let base = price(&fg, vec![]);
        let b = price(&with_callout(fg.clone(), 0, spec), seeded_rates());
        eprintln!(
            "\n{name}: baseline total={:.4}  +tolerance_cost={:.4}  ⇒ total={:.4}",
            base.total_price, b.tolerance_cost, b.total_price
        );
        for l in b.reasoning_log.iter().filter(|l| l.contains("[tolerance]")) {
            eprintln!("    {l}");
        }
    }
}
