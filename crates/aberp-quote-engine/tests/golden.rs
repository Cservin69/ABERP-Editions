//! Locked numeric snapshot — ANY algorithm change breaks this test
//! and forces a conscious update.
//!
//! Treat this file as a hash of the v1 algorithm. If a future PR
//! changes the numbers, the diff MUST be reviewed (does the new
//! number reflect the new intent?) and the golden values bumped in
//! the same commit — never silently.

mod common;

use aberp_quote_engine::quote;
use common::*;

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

#[test]
fn fixed_input_produces_locked_numeric_output_at_4dp() {
    // Setup is `simple_feature_graph` + `default_*` fixtures with
    // qty=10 + Standard tolerance, under the S418 geometry model
    // (report §5/§8 day-1 params; surface_area_mm2=0 → bbox fallback).
    //
    // Hand-derivation (matches the algorithm in `src/engine.rs`):
    //   bbox 50×30×20 = 30000 mm³; stock = 30000 * 1.15 = 34500
    //   mass_kg       = 34500 * 2.70 / 1e6        = 0.09315
    //   material_cost = 0.09315 * 8.0             = 0.7452
    //   (no stock adj, 6061 not exotic)
    //
    //   removed_cm3   = (34500 - 25000)/1000      = 9.5
    //   roughing_min  = 9.5 * 1.0 / 8.0           = 1.1875
    //   bbox_area     = 2*(1500+600+1000)         = 6200 mm² = 62 cm²
    //   finishing_min = 62 * 0.08 * 1.0           = 4.96
    //   feature time  = Hole(2*4) + Pocket(2*1)   = 10
    //   machining_minutes = 1.1875 + 4.96 + 10    = 16.1475
    //   inspection    = 0 (Standard tol)
    //   machining_cost = 16.1475 * 1.6667 * 1.0   = 26.91303825
    //
    //   setup_minutes = 20 + 0 + (5+5 rule)       = 30
    //   setup_cost    = 30 * 1.6667 / 10          = 5.0001  (qty 10 ≥ 5)
    //
    //   cad_cam: base 1.0 (fill 0.833 ≥0.60, no flags, soft material)
    //   cad_cam_cost  = 1.0 * 100 / 10            = 10.0
    //
    //   subtotal = 0.7452 + 26.91303825 + 5.0001 + 10.0 = 42.65833825
    //   overhead = * 0.20                          = 8.5316677
    //   margin   = (subtotal+overhead) * 0.35      = 17.9165021
    //   total    = 69.1065080
    //   actual margin/total ≈ 0.2593 (> 0.10 floor)

    let materials = vec![default_material("6061-T6")];
    let fg = simple_feature_graph("6061-T6");
    let r = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .expect("golden quote must succeed");

    assert_eq!(round4(r.material_cost), 0.7452, "material_cost");
    assert_eq!(round4(r.machining_cost), 26.9130, "machining_cost");
    assert_eq!(round4(r.cad_cam_cost), 10.0000, "cad_cam_cost");
    assert_eq!(round4(r.setup_cost), 5.0001, "setup_cost");
    assert_eq!(round4(r.overhead), 8.5317, "overhead");
    assert_eq!(round4(r.margin), 17.9165, "margin");
    assert_eq!(round4(r.total_price), 69.1065, "total_price");
    assert_eq!(round4(r.machining_minutes), 16.1475, "machining_minutes");
    assert_eq!(round4(r.inspection_minutes), 0.0000, "inspection_minutes");
    assert!(!r.route_to_5_axis);

    // S1/ADR-0094 back-compat tripwire: the default (RectangularBlock)
    // stock form must emit TODAY'S EXACT material line, byte-for-byte —
    // not merely the same numbers. Guards the reasoning_log contract.
    assert!(
        r.reasoning_log
            .iter()
            .any(|l| l == "[material] bbox 50.000×30.000×20.000 = bbox_volume_mm3=30000.0000 * (1 + scrap_factor=0.1500) = stock_volume_mm3=34500.0000"),
        "RectangularBlock must reproduce the pre-S1 material line exactly"
    );
}
