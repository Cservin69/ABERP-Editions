//! ADR-0097 Part 2 / T3 — new goldens for the additive `tolerance_cost` model.
//!
//! The inert-by-default mechanism (serde defaults + EMPTY `tolerance_cost_rates`
//! slice) protects *old* prices (see `golden.rs`/`determinism.rs`/`branches.rs`/
//! `property.rs`, all byte-identical); THESE fixtures protect the *new*
//! tolerance math: per-critical-feature in-process + CMM inspection, extra
//! slower-feed finishing passes, the tightest-band grinding adder, the
//! scrap/rework uplift, the per-drawing manual-review flag, and the
//! zero-contribution-seed inert proof. Hand-derived to 4 dp; each test's
//! arithmetic chain is in its comment.
//!
//! Base part = `simple_feature_graph("6061-T6")` + `default_*` fixtures, qty 10,
//! resolved `target_tolerance = Standard`, empty `machine_rates` ⇒ the effective
//! EUR/min is the global `machining_rate_eur_per_minute = 1.6667`, and (from
//! `golden.rs`) `material_cost = 0.7452`, `machining_cost = 26.9130`,
//! `base_finish_min (finishing_min) = 4.96`. A per-critical-feature callout does
//! NOT move the machining line (that keys on `target_tolerance = Standard`); it
//! drives only the new additive `tolerance_cost` line.

mod common;

use aberp_quote_engine::{
    quote_with_catalogue, CalibrationTable, FeatureTolerance, MachineRate, QuoteBreakdown,
    ToleranceCostRate, ToleranceSpec,
};
use common::*;

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
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

fn callout(feature_index: usize, spec: ToleranceSpec) -> FeatureTolerance {
    FeatureTolerance {
        feature_index,
        spec,
    }
}

/// Price the base part with the given per-critical-feature callouts, tolerance
/// cost-rate table, and machine rates (for the grinder rate). qty 10, resolved
/// `target_tolerance = Standard` (the inert default band).
fn quote_tol(
    crit: Vec<FeatureTolerance>,
    rates: Vec<ToleranceCostRate>,
    machine_rates: Vec<MachineRate>,
) -> QuoteBreakdown {
    let mut fg = simple_feature_graph("6061-T6");
    fg.critical_feature_tolerances = crit;
    let mut fx = CatalogueFixture::new("6061-T6");
    fx.tolerance_cost_rates = rates;
    fx.machine_rates = machine_rates;
    let snap = fx.snapshot();
    quote_with_catalogue(
        &fg,
        &snap,
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
        &CalibrationTable::neutral(),
    )
    .expect("tolerance quote must succeed")
}

/// A fully-seeded, non-trivial cost-rate table (one row per band). Chosen for
/// clean hand-arithmetic, not production gospel (the operator tunes
/// `quoting_tolerance_cost_rates`; the boot seed is zero-contribution — Q6).
fn seeded_rates() -> Vec<ToleranceCostRate> {
    vec![
        rate("loose", 0.0, 0.0, 0.0, 0.0, 1.0, false),
        rate("standard", 0.0, 0.0, 0.0, 0.0, 1.0, false),
        // Tight: scrap uplift only.
        rate("tight", 0.0, 0.0, 0.0, 0.10, 1.0, false),
        // Precision: finishing + inspection + scrap (the worked Ø12 H7 case).
        rate("precision", 1.0, 2.0, 3.0, 0.05, 1.5, false),
        // UltraPrecision: grinding escalation only (isolated).
        rate("ultra_precision", 0.0, 0.0, 0.0, 0.0, 1.0, true),
    ]
}

#[test]
fn tight_critical_bore_itemised_nonzero() {
    // A Ø12 H7 (IT7) critical bore on feature #0. IT7 -> Precision band ⇒ the
    // governing band is Precision (tighter than the Standard overall). n=1.
    //   inspection = (2.0 in-proc + 3.0 CMM) * 1 feat = 5.0 min * 1.6667 = 8.3335
    //   finishing  = 1.0 * base_finish 4.96 * feed 1.5 = 7.44 min * 1.6667 = 12.4002
    //   grinding   = 0 (Precision is not the tightest band)
    //   scrap      = 0.05 * (0.7452 + 26.91303825) = 0.05 * 27.65823825 = 1.3829
    //   tolerance_cost = 8.3335 + 12.400248 + 1.3829119 = 22.1167
    let b = quote_tol(
        vec![callout(0, ToleranceSpec::ItGrade { grade: 7 })],
        seeded_rates(),
        vec![],
    );
    assert_eq!(
        round4(b.tolerance_cost),
        22.1167,
        "tight bore tolerance_cost"
    );

    // The legacy machining line is UNTOUCHED (keys on target=Standard).
    assert_eq!(
        round4(b.machining_cost),
        26.9130,
        "machining_cost unchanged"
    );

    // Every term reconstructs from the reasoning log (trust signal).
    let log = &b.reasoning_log;
    assert!(log.iter().any(|l| l.contains("governing band=precision")));
    assert!(log
        .iter()
        .any(|l| l.contains("critical feature #0: tolerance: IT7 -> precision band")));
    assert!(log.iter().any(|l| l
        .contains("(2.0000 in-proc + 3.0000 CMM) min/feat * 1 feat = 5.0000 min")
        && l.contains("= 8.3335 EUR")));
    assert!(log.iter().any(|l| l.contains(
        "finish_passes_add=1.0000 * base_finish_min=4.9600 * feed_slowdown=1.5000 = 7.4400 min"
    ) && l.contains("= 12.4002 EUR")));
    assert!(log.iter().any(|l| l.contains(
        "scrap/rework = rework_scrap_pct=0.0500 * (material=0.7452 + machining=26.9130) = 1.3829 EUR"
    )));
    assert!(log
        .iter()
        .any(|l| l.contains("total tolerance_cost=22.1167 EUR")));
    // The line folds into the subtotal (totals line names the tolerance term).
    assert!(log
        .iter()
        .any(|l| l.contains("+ tolerance=22.1167 = subtotal=")));
    // Decomposition adds up to the line, to 4 dp.
    assert_eq!(round4(8.3335 + 12.400248 + 1.3829119125), 22.1167);
}

#[test]
fn ultraprecision_ground_feature_grinding_adder() {
    // A critical feature at IT4 (<= IT5) -> UltraPrecision (the tightest band).
    // The UP row sets grinding_escalation; a Grinder machine-rate row prices it.
    //   grinding = 12.0 min/feat * 1 feat * grinder_rate 2.5 = 30.0 EUR
    //   all other UP terms are zero ⇒ tolerance_cost = 30.0000
    let grinder = MachineRate {
        family: "grinder".to_string(),
        attended_rate_eur_per_min: 2.5,
        lights_out_factor: 1.0,
        unattended_capable: false,
    };
    let b = quote_tol(
        vec![callout(0, ToleranceSpec::ItGrade { grade: 4 })],
        seeded_rates(),
        vec![grinder.clone()],
    );
    assert_eq!(round4(b.tolerance_cost), 30.0000, "grinding adder");
    assert_eq!(
        round4(b.machining_cost),
        26.9130,
        "machining_cost unchanged"
    );
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("governing band=ultra_precision")));
    assert!(b.reasoning_log.iter().any(|l| l.contains(
        "grinding escalation (band=ultra_precision): 12.0000 min (12.0000/feat * 1 feat) * grinder_rate=2.5000 EUR/min = 30.0000 EUR"
    )));

    // Grinding fires ONLY at the tightest band: the same grinding_escalation
    // flag on a Precision row (IT7) does NOT add a grinding term.
    let prec_grind = vec![rate("precision", 0.0, 0.0, 0.0, 0.0, 1.0, true)];
    let b2 = quote_tol(
        vec![callout(0, ToleranceSpec::ItGrade { grade: 7 })],
        prec_grind,
        vec![grinder],
    );
    assert_eq!(
        b2.tolerance_cost, 0.0,
        "grinding must not fire below the tightest band"
    );
    assert!(!b2
        .reasoning_log
        .iter()
        .any(|l| l.contains("grinding escalation")));
}

#[test]
fn scrap_uplift_case() {
    // A critical feature at IT8 -> Tight band; the Tight row carries a 10%
    // scrap/rework uplift only.
    //   scrap = 0.10 * (0.7452 + 26.91303825) = 0.10 * 27.65823825 = 2.7658
    let b = quote_tol(
        vec![callout(1, ToleranceSpec::ItGrade { grade: 8 })],
        seeded_rates(),
        vec![],
    );
    assert_eq!(
        round4(b.tolerance_cost),
        2.7658,
        "scrap uplift tolerance_cost"
    );
    assert!(b.reasoning_log.iter().any(|l| l.contains(
        "scrap/rework = rework_scrap_pct=0.1000 * (material=0.7452 + machining=26.9130) = 2.7658 EUR"
    )));
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("governing band=tight")));
}

#[test]
fn default_unspecified_empty_rates_is_byte_inert() {
    // The inert default: no callouts, EMPTY rate slice, resolved Standard ⇒
    // tolerance_cost 0.0, NO `[tolerance` line, and the totals line is the
    // no-tolerance (and no-gear) format — exactly today's bytes.
    let b = quote_tol(vec![], vec![], vec![]);
    assert_eq!(b.tolerance_cost, 0.0);
    assert!(!b.reasoning_log.iter().any(|l| l.contains("[tolerance")));
    assert!(b.reasoning_log.iter().any(|l| {
        l.contains("+ cad_cam=") && l.contains("= subtotal=") && !l.contains("tolerance")
    }));
    // Byte-for-byte today's golden numbers are preserved.
    assert_eq!(round4(b.machining_cost), 26.9130);
    assert_eq!(round4(b.total_price), 69.1065);
}

#[test]
fn zero_contribution_seed_prices_zero() {
    // A FULLY-SEEDED but zero-contribution table (all rows all-zero) + no
    // callouts ⇒ tolerance_cost 0.0 ⇒ the subtotal line stays the no-tolerance
    // format. This is the boot-seed posture (Q6): rows exist to edit, nothing
    // moves until the operator tunes them.
    let zero = vec![
        rate("loose", 0.0, 0.0, 0.0, 0.0, 1.0, false),
        rate("standard", 0.0, 0.0, 0.0, 0.0, 1.0, false),
        rate("tight", 0.0, 0.0, 0.0, 0.0, 1.0, false),
        rate("precision", 0.0, 0.0, 0.0, 0.0, 1.0, false),
        rate("ultra_precision", 0.0, 0.0, 0.0, 0.0, 1.0, false),
    ];
    let b = quote_tol(vec![], zero, vec![]);
    assert_eq!(
        b.tolerance_cost, 0.0,
        "zero-contribution seed must price zero"
    );
    assert!(b.reasoning_log.iter().any(|l| {
        l.contains("+ cad_cam=") && l.contains("= subtotal=") && !l.contains("tolerance")
    }));
    assert_eq!(
        round4(b.total_price),
        69.1065,
        "total unchanged under zero seed"
    );
}

#[test]
fn per_drawing_default_band_and_flag() {
    // "Per drawing" (GD&T) resolves to the DEFAULT band and raises the
    // manual-review flag — never silently tightened (Q5).

    // (a) With NO rate table: tolerance_cost == 0 (inert), nothing tightened.
    let b0 = quote_tol(vec![callout(0, ToleranceSpec::PerDrawing)], vec![], vec![]);
    assert_eq!(b0.tolerance_cost, 0.0, "per-drawing + no rates ⇒ zero cost");

    // (b) With a seeded table where the Standard (default) row carries a 5%
    // scrap uplift: the governing band stays Standard (NOT tightened), the
    // manual-review flag is surfaced, and it prices at the default band.
    //   scrap = 0.05 * (0.7452 + 26.91303825) = 1.3829
    let rates = vec![
        rate("standard", 0.0, 0.0, 0.0, 0.05, 1.0, false),
        rate("precision", 9.0, 9.0, 9.0, 0.9, 9.0, true),
        rate("ultra_precision", 9.0, 9.0, 9.0, 0.9, 9.0, true),
    ];
    let b = quote_tol(vec![callout(0, ToleranceSpec::PerDrawing)], rates, vec![]);
    assert_eq!(
        round4(b.tolerance_cost),
        1.3829,
        "per-drawing priced at default band"
    );
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("governing band=standard")));
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("MANUAL REVIEW (per-drawing GD&T")));
    // Never silently tightened to precision/ultra (whose rows are large).
    assert!(!b
        .reasoning_log
        .iter()
        .any(|l| l.contains("governing band=precision")
            || l.contains("governing band=ultra_precision")));
}

#[test]
fn missing_band_row_is_fail_soft_zero_with_loud_log() {
    // A non-empty table that lacks the governing band's row ⇒ fail-soft 0.0 +
    // a loud line (CLAUDE.md rule 12), mirroring the gear missing-rate path.
    let only_loose = vec![rate("loose", 1.0, 1.0, 1.0, 0.1, 1.0, false)];
    let b = quote_tol(
        vec![callout(0, ToleranceSpec::ItGrade { grade: 7 })], // -> precision
        only_loose,
        vec![],
    );
    assert_eq!(b.tolerance_cost, 0.0);
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("WARNING no ToleranceCostRate row for band=precision")));
}

#[test]
fn sweep_tolerance_cost_finite_and_non_negative() {
    // Property-style sweep (kept here so the locked `tests/property.rs` stays
    // byte-identical): across bands, specs and callout counts the new line is
    // always finite and >= 0.
    let specs = [
        ToleranceSpec::Unspecified,
        ToleranceSpec::GeneralClass {
            class: aberp_quote_engine::GeneralClass::Iso2768Fine,
        },
        ToleranceSpec::ItGrade { grade: 4 },
        ToleranceSpec::ItGrade { grade: 11 },
        ToleranceSpec::PlusMinus { value_mm: 0.01 },
        ToleranceSpec::PerDrawing,
    ];
    let grinder = vec![MachineRate {
        family: "grinder".to_string(),
        attended_rate_eur_per_min: 3.0,
        lights_out_factor: 1.0,
        unattended_capable: false,
    }];
    for spec in specs {
        for n in 0..3usize {
            let crit: Vec<FeatureTolerance> = (0..n).map(|_| callout(0, spec)).collect();
            let b = quote_tol(crit, seeded_rates(), grinder.clone());
            assert!(b.tolerance_cost.is_finite(), "finite for {spec:?} n={n}");
            assert!(b.tolerance_cost >= 0.0, "non-negative for {spec:?} n={n}");
            assert!(b.total_price.is_finite() && b.total_price > 0.0);
        }
    }
}
