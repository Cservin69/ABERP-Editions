//! Five branch tests covering each major scoring path the brief
//! named: baseline (no thin wall, standard tol), thin-wall +
//! standard tol (no bump), thin-wall + precision tol (bump fires),
//! 5-axis required, exotic material.

mod common;

use aberp_quote_engine::{quote, ToleranceRange};
use common::*;

#[test]
fn baseline_no_thin_wall_standard_tol() {
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
    .expect("baseline quote should succeed");

    assert!(r.material_cost > 0.0);
    assert!(r.labor_cost > 0.0);
    assert!(r.total_price > 0.0);
    assert!(!r.route_to_5_axis);
    assert!(
        r.reasoning_log.iter().any(|line| line.contains("no bump")),
        "log should explicitly state no thin-wall bump fired",
    );
    // margin / total >= min_margin
    assert!(r.margin / r.total_price >= default_parameters().min_margin);
}

#[test]
fn thin_wall_present_standard_tol_no_bump() {
    let materials = vec![default_material("6061-T6")];
    let mut fg = simple_feature_graph("6061-T6");
    fg.thin_wall_present = true;
    let r = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        ToleranceRange::Standard,
    )
    .expect("thin-wall + Standard should succeed");

    // No bump because tolerance < Tight.
    assert!(r
        .reasoning_log
        .iter()
        .any(|l| l.contains("no bump") && l.contains("thin_wall_present=true")));
}

#[test]
fn thin_wall_present_precision_tol_applies_bump() {
    let materials = vec![default_material("6061-T6")];
    let mut fg = simple_feature_graph("6061-T6");
    fg.thin_wall_present = true;

    // Same setup, two tolerances — compare labor_cost ratio.
    let r_std = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        ToleranceRange::Standard,
    )
    .unwrap();

    let r_prec = quote(
        &fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        ToleranceRange::Precision,
    )
    .unwrap();

    // Precision labor incorporates: tolerance_mult (1.9 vs 1.0),
    // inspection minutes (1.5 vs 0.0 per feature), AND the
    // THIN_WALL_TIGHT_TOL_BUMP=1.15. Just assert "precision > std"
    // and the bump line appears in the log.
    assert!(
        r_prec.labor_cost > r_std.labor_cost,
        "Precision labor must exceed Standard"
    );
    assert!(r_prec
        .reasoning_log
        .iter()
        .any(|l| l.contains("THIN_WALL_TIGHT_TOL_BUMP")));
}

#[test]
fn requires_5_axis_sets_routing_flag() {
    let materials = vec![default_material("6061-T6")];
    let mut fg = simple_feature_graph("6061-T6");
    fg.requires_5_axis = true;

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
    .unwrap();

    assert!(r.route_to_5_axis);
    assert!(r
        .reasoning_log
        .iter()
        .any(|l| l.contains("route_to_5_axis=true")));
}

#[test]
fn exotic_material_applies_tax() {
    // Same volume + quantity, swap material grade. The exotic
    // surcharge must make material_cost strictly larger after
    // normalising for density+cost.
    let mats = vec![default_material("6061-T6"), exotic_material("Inconel 718")];
    let fg_alu = simple_feature_graph("6061-T6");
    let fg_inc = simple_feature_graph("Inconel 718");

    let r_alu = quote(
        &fg_alu,
        &mats,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .unwrap();

    let r_inc = quote(
        &fg_inc,
        &mats,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .unwrap();

    assert!(
        r_inc.material_cost > r_alu.material_cost,
        "Inconel must price above 6061 base"
    );
    assert!(r_inc
        .reasoning_log
        .iter()
        .any(|l| l.contains("exotic-material tax")));
    assert!(r_alu.reasoning_log.iter().any(|l| l.contains("not exotic")));
}
