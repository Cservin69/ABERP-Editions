//! ADR-0094 Q2 / ADR-0097 T1 — `CatalogueSnapshot` equivalence.
//!
//! The new `quote_with_catalogue` superset entry and the legacy
//! `quote_with_shop_model` positional entry are the SAME computation: the
//! former just bundles the six catalogue slices into one borrowed struct.
//! These tests prove the T1 refactor is byte-identical — same
//! `QuoteBreakdown`, same `reasoning_log` — for both the inert (empty
//! machine/gear slices) and a populated catalogue.

mod common;

use aberp_quote_engine::{
    quote_with_catalogue, quote_with_shop_model, CalibrationTable, CatalogueSnapshot, MachineRate,
};
use common::*;

#[test]
fn catalogue_entry_matches_shop_model_entry_inert() {
    let fixture = CatalogueFixture::new("6061-T6");
    let fg = simple_feature_graph("6061-T6");
    let params = default_parameters();
    let cal = CalibrationTable::neutral();

    let via_positional = quote_with_shop_model(
        &fg,
        &fixture.materials,
        &fixture.complexity_rules,
        &fixture.tolerance_multipliers,
        &fixture.stock_adjustments,
        &params,
        DEFAULT_QTY,
        DEFAULT_TOL,
        &cal,
        &fixture.machine_rates,
        &fixture.gear_process_rates,
    )
    .expect("positional entry must price");

    let via_snapshot = quote_with_catalogue(
        &fg,
        &fixture.snapshot(),
        &params,
        DEFAULT_QTY,
        DEFAULT_TOL,
        &cal,
    )
    .expect("snapshot entry must price");

    assert_eq!(
        via_positional, via_snapshot,
        "quote_with_catalogue must reproduce quote_with_shop_model byte-for-byte"
    );
}

#[test]
fn catalogue_entry_plumbs_machine_rates_identically() {
    // A populated machine-rate slice proves the snapshot carries the
    // catalogue slices through to the body exactly as the positional args
    // did: both entries receive identical inputs => identical output.
    let fixture = CatalogueFixture::new("6061-T6");
    let fg = simple_feature_graph("6061-T6");
    let params = default_parameters();
    let cal = CalibrationTable::neutral();
    let machine_rates = vec![MachineRate {
        family: "swiss-turn-mill".to_string(),
        attended_rate_eur_per_min: 1.2,
        lights_out_factor: 0.6,
        unattended_capable: true,
    }];

    let via_positional = quote_with_shop_model(
        &fg,
        &fixture.materials,
        &fixture.complexity_rules,
        &fixture.tolerance_multipliers,
        &fixture.stock_adjustments,
        &params,
        DEFAULT_QTY,
        DEFAULT_TOL,
        &cal,
        &machine_rates,
        &fixture.gear_process_rates,
    )
    .expect("positional entry must price");

    let snapshot = CatalogueSnapshot {
        materials: &fixture.materials,
        complexity_rules: &fixture.complexity_rules,
        tolerance_multipliers: &fixture.tolerance_multipliers,
        stock_adjustments: &fixture.stock_adjustments,
        machine_rates: &machine_rates,
        gear_process_rates: &fixture.gear_process_rates,
    };
    let via_snapshot =
        quote_with_catalogue(&fg, &snapshot, &params, DEFAULT_QTY, DEFAULT_TOL, &cal)
            .expect("snapshot entry must price");

    assert_eq!(
        via_positional, via_snapshot,
        "snapshot machine_rates must price identically to the positional arg"
    );
}
