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
    // qty=10 + Standard tolerance.
    //
    // Hand-derivation (matches the algorithm in `src/engine.rs`):
    //   scrap_volume    = 25000 * 1.08            = 27000
    //   mass_kg         = 27000 * 2.70 / 1e6      = 0.0729
    //   material_cost   = 0.0729 * 8.0            = 0.5832
    //   (no stock adj, not exotic)
    //
    //   features:
    //     Hole/XS/count=4: 2.0 * 4 * 1.0 = 8 min, rule#2  setup=5
    //     Pocket/S/count=1: 2.0 * 1 * 1.0 = 2 min, rule#7 setup=5
    //   machining_minutes = 10
    //   unique-rule setup_penalty = 5 + 5 = 10 min
    //   inspection_minutes = 0 (Standard tol)
    //
    //   labor_cost = (10 / 1.2 + 0) * 1.50 * 1.0  = 12.5
    //   (no thin-wall bump; quote_multiplier=1)
    //
    //   setup_cost = 10 * 1.50 / 10               = 1.5     (qty=10 >= threshold=5)
    //
    //   subtotal   = 0.5832 + 12.5 + 1.5          = 14.5832
    //   overhead   = 14.5832 * 0.20               = 2.91664
    //   margin     = (14.5832 + 2.91664) * 0.35   = 6.124944
    //   total      = 23.624784
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

    assert_eq!(round4(r.material_cost), 0.5832, "material_cost");
    assert_eq!(round4(r.labor_cost), 12.5000, "labor_cost");
    assert_eq!(round4(r.setup_cost), 1.5000, "setup_cost");
    assert_eq!(round4(r.overhead), 2.9166, "overhead");
    assert_eq!(round4(r.margin), 6.1249, "margin");
    assert_eq!(round4(r.total_price), 23.6248, "total_price");
    assert_eq!(round4(r.machining_minutes), 10.0000, "machining_minutes");
    assert_eq!(round4(r.inspection_minutes), 0.0000, "inspection_minutes");
    assert!(!r.route_to_5_axis);
}
