//! S5 / ADR-0094 Gap 3 — new goldens for the gear-generation op model.
//!
//! The inert-by-default mechanism (serde defaults + empty `gears`) protects
//! *old* prices (see `golden.rs`/`determinism.rs`); THESE fixtures protect the
//! *new* gear math: external teeth hobbed vs power-skived in-cycle, the
//! internal-ring wire-EDM premium, deterministic `Auto` selection by routed
//! family + AGMA, multi-gear summation, and the fail-soft missing-rate path.
//! Hand-derived to 4 dp; each test's arithmetic chain is in its comment.
//!
//! Rate basis: `machine_rates` is empty in every case ⇒ the effective €/min is
//! the global `machining_rate_eur_per_minute` = 1.6667, so the gear cost is
//! isolated from the Gap-2 family-rate machinery and stays hand-checkable.

mod common;

use aberp_quote_engine::{
    quote_with_shop_model, select_gear_process, CalibrationTable, GearKind, GearOp, GearProcess,
    GearProcessRate, MachineFamily, QuoteBreakdown, StockForm, ToleranceRange,
};
use common::*;

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// Seed gear-process coefficients. Chosen for clean hand-arithmetic, not as
/// production gospel (the operator tunes `quoting_gear_processes`; S429-style
/// calibration can later learn them — ADR-0094 Q5).
fn gear_rates() -> Vec<GearProcessRate> {
    let r = |process: &str, setup, mpt, mexp, agma, icf| GearProcessRate {
        process: process.to_string(),
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

fn gear(kind: GearKind, module_mm: f64, teeth: u32, face_width_mm: f64, agma: u8) -> GearOp {
    GearOp {
        kind,
        module_mm,
        teeth,
        face_width_mm,
        quality_agma: agma,
        process: GearProcess::Auto,
    }
}

/// Quote with the given stock form + gears, global rate (empty machine_rates),
/// the seed gear rates, qty 10, Standard tol.
fn quote_geared(stock: StockForm, gears: Vec<GearOp>, rates: &[GearProcessRate]) -> QuoteBreakdown {
    let mut fg = simple_feature_graph("6061-T6");
    fg.stock_form = stock;
    fg.gears = gears;
    quote_with_shop_model(
        &fg,
        &[default_material("6061-T6")],
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        10,
        ToleranceRange::Standard,
        &CalibrationTable::neutral(),
        &[],
        rates,
    )
    .expect("geared quote must succeed")
}

#[test]
fn auto_selects_process_by_kind_family_and_quality() {
    use GearKind::*;
    use GearProcess::*;
    // External ⇒ skive on a turning family, hob otherwise.
    assert_eq!(
        select_gear_process(ExternalSpurHelical, MachineFamily::SwissTurnMill, 8),
        PowerSkive
    );
    assert_eq!(
        select_gear_process(ExternalSpurHelical, MachineFamily::TurnMill, 8),
        PowerSkive
    );
    assert_eq!(
        select_gear_process(ExternalSpurHelical, MachineFamily::ThreeAxisMill, 8),
        Hob
    );
    assert_eq!(
        select_gear_process(ExternalSpurHelical, MachineFamily::FiveAxisMill, 12),
        Hob
    );
    // Internal ⇒ shape, escalating to wire-EDM STRICTLY above the AGMA datum 12.
    assert_eq!(
        select_gear_process(InternalRing, MachineFamily::ThreeAxisMill, 12),
        Shape
    );
    assert_eq!(
        select_gear_process(InternalRing, MachineFamily::ThreeAxisMill, 13),
        WireEdm
    );
    assert_eq!(
        select_gear_process(InternalRing, MachineFamily::SwissTurnMill, 5),
        Shape
    );
}

#[test]
fn external_hob_vs_skive_in_cycle() {
    // Same external gear: m=2, z=20, b=20mm (fw_factor=2.0), AGMA 8 (qf=1.0).
    let g = || gear(GearKind::ExternalSpurHelical, 2.0, 20, 20.0, 8);

    // (a) Prismatic block ⇒ ThreeAxisMill ⇒ Auto picks HOB (standalone):
    //   gen = 20·0.30·2^1·2.0·1.0 = 24.0; gear_min = 20 + 24 = 44.0
    //   cost = 44.0 · 1.6667 = 73.3348
    let hob = quote_geared(StockForm::RectangularBlock, vec![g()], &gear_rates());
    assert_eq!(round4(hob.gear_cost), 73.3348, "hob external gear");
    assert!(hob.reasoning_log.iter().any(|l| l.contains("selected hob")));

    // (b) Round bar Ø20 ≤ bar_capacity 32 ⇒ SwissTurnMill ⇒ Auto picks
    //     POWER-SKIVE in-cycle:
    //   gen = 20·0.10·2·2.0·1.0 = 8.0; base = 8 + 8 = 16.0; ·in_cycle 0.5 = 8.0
    //   cost = 8.0 · 1.6667 = 13.3336
    let skive = quote_geared(
        StockForm::RoundBar {
            diameter_mm: 20.0,
            length_mm: 60.0,
        },
        vec![g()],
        &gear_rates(),
    );
    assert_eq!(
        round4(skive.gear_cost),
        13.3336,
        "power-skived external gear"
    );
    assert!(skive
        .reasoning_log
        .iter()
        .any(|l| l.contains("selected power_skive")));
    assert!(skive
        .reasoning_log
        .iter()
        .any(|l| l.contains("in-cycle on swiss-turn-mill")));

    // The whole point of Gap 3: in-cycle skiving is far cheaper than hobbing.
    assert!(skive.gear_cost < hob.gear_cost);
}

#[test]
fn internal_ring_wire_edm_premium_over_shape() {
    // Internal ring, m=3, z=80, b=15mm (fw_factor=1.5), on a prismatic route.
    // (a) AGMA 10 (≤12) ⇒ SHAPE: qf = 1 + (10-8)·0.15 = 1.30
    //   gen = 80·0.50·3·1.5·1.30 = 234.0; gear_min = 30 + 234 = 264.0
    //   cost = 264.0 · 1.6667 = 440.0088
    let shape = quote_geared(
        StockForm::RectangularBlock,
        vec![gear(GearKind::InternalRing, 3.0, 80, 15.0, 10)],
        &gear_rates(),
    );
    assert_eq!(round4(shape.gear_cost), 440.0088, "shaped internal ring");
    assert!(shape
        .reasoning_log
        .iter()
        .any(|l| l.contains("selected shape")));

    // (b) AGMA 13 (>12) ⇒ WIRE-EDM: qf = 1 + (13-8)·0.20 = 2.0
    //   gen = 80·2.0·3·1.5·2.0 = 1440.0; gear_min = 15 + 1440 = 1455.0
    //   cost = 1455.0 · 1.6667 = 2425.0485
    let edm = quote_geared(
        StockForm::RectangularBlock,
        vec![gear(GearKind::InternalRing, 3.0, 80, 15.0, 13)],
        &gear_rates(),
    );
    assert_eq!(round4(edm.gear_cost), 2425.0485, "wire-EDM internal ring");
    assert!(edm
        .reasoning_log
        .iter()
        .any(|l| l.contains("selected wire_edm")));

    // The tightest internal ring carries an explicit, large EDM premium.
    assert!(edm.gear_cost > 5.0 * shape.gear_cost);
}

#[test]
fn planetary_multi_gear_sums_each_op() {
    // A planetary-style cluster on a Ø25 bar (≤32 ⇒ SwissTurnMill):
    //   sun  (external m2 z10 b10 AGMA8) ⇒ skive: gen 10·0.10·2·1·1=2.0;
    //        base 8+2=10; ·0.5 = 5.0; cost 5.0·1.6667 = 8.3335
    //   planet (external m2 z10 b10 AGMA8) ⇒ skive: identical = 8.3335
    //   ring (internal m2 z50 b10 AGMA10) ⇒ shape: qf=1+2·0.15=1.30;
    //        gen 50·0.50·2·1·1.30=65.0; base 30+65=95.0; cost 95·1.6667=158.3365
    //   Σ gear_cost = 8.3335 + 8.3335 + 158.3365 = 175.0035
    let b = quote_geared(
        StockForm::RoundBar {
            diameter_mm: 25.0,
            length_mm: 80.0,
        },
        vec![
            gear(GearKind::ExternalSpurHelical, 2.0, 10, 10.0, 8),
            gear(GearKind::ExternalSpurHelical, 2.0, 10, 10.0, 8),
            gear(GearKind::InternalRing, 2.0, 50, 10.0, 10),
        ],
        &gear_rates(),
    );
    assert_eq!(round4(b.gear_cost), 175.0035, "summed planetary gear_cost");
    // Two external skives + one internal shape are each logged.
    assert_eq!(
        b.reasoning_log
            .iter()
            .filter(|l| l.contains("selected power_skive"))
            .count(),
        2
    );
    assert_eq!(
        b.reasoning_log
            .iter()
            .filter(|l| l.contains("selected shape"))
            .count(),
        1
    );
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("total gear_cost=175.0035")));
    // gear_cost folds into the subtotal (totals line names the gear term).
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("+ gear=175.0035 = subtotal=")));
}

#[test]
fn forced_process_overrides_auto() {
    // Operator forces BROACH on an internal ring that Auto would shape.
    //   m=2, z=50, b=10mm (fw 1.0), AGMA 10 ⇒ broach qf=1+(10-8)·0.10=1.20
    //   gen = 50·0.05·2·1·1.20 = 6.0; gear_min = 60 + 6 = 66.0
    //   cost = 66.0 · 1.6667 = 110.0022
    let mut g = gear(GearKind::InternalRing, 2.0, 50, 10.0, 10);
    g.process = GearProcess::Broach;
    let b = quote_geared(StockForm::RectangularBlock, vec![g], &gear_rates());
    assert_eq!(round4(b.gear_cost), 110.0022, "operator-forced broach");
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("process=broach (operator-forced)")));
}

#[test]
fn missing_process_rate_is_fail_soft_zero_with_loud_log() {
    // A round-bar external gear Auto-selects power_skive, but the catalogue
    // only seeds `hob` ⇒ no matching row ⇒ this gear contributes 0.0 EUR and a
    // LOUD reasoning line (CLAUDE.md rule 12: surfaced, not silent).
    let rates = vec![GearProcessRate {
        process: "hob".to_string(),
        setup_min: 20.0,
        min_per_tooth: 0.30,
        module_exponent: 1.0,
        agma_quality_factor_base: 0.10,
        in_cycle_factor: 1.0,
    }];
    let b = quote_geared(
        StockForm::RoundBar {
            diameter_mm: 20.0,
            length_mm: 60.0,
        },
        vec![gear(GearKind::ExternalSpurHelical, 2.0, 20, 20.0, 8)],
        &rates,
    );
    assert_eq!(b.gear_cost, 0.0);
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("WARNING no GearProcessRate row for process=power_skive")));
}

#[test]
fn empty_gears_add_no_cost_and_no_gear_log() {
    // Even WITH a fully-seeded gear catalogue, a part with no gears prices
    // exactly as today: gear_cost 0.0, no `[gear` line, and the totals line is
    // the no-gear format (no "+ gear=").
    let b = quote_geared(StockForm::RectangularBlock, vec![], &gear_rates());
    assert_eq!(b.gear_cost, 0.0);
    assert!(!b.reasoning_log.iter().any(|l| l.contains("[gear")));
    assert!(b
        .reasoning_log
        .iter()
        .any(|l| l.contains("+ cad_cam=") && l.contains("= subtotal=") && !l.contains("gear")));
}
