//! S3 / ADR-0094 Gap 2 — machine-family rate, lights-out factor, and
//! geometry routing. Proves: (a) `route_family` picks the family from
//! geometry; (b) the lights-out effective rate scales the machining cost
//! exactly; (c) the new closed-vocab families round-trip; and — the
//! load-bearing back-compat claim — (d) an empty (or non-matching) rate
//! table reproduces the global flat-rate price byte-for-byte.

mod common;

use aberp_quote_engine::{
    quote, quote_with_shop_model, route_family, CalibrationTable, FeatureGraph, MachineFamily,
    MachineRate, QuoteBreakdown, StockForm,
};
use common::*;

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn graph(f: impl FnOnce(&mut FeatureGraph)) -> FeatureGraph {
    let mut fg = simple_feature_graph("6061-T6");
    f(&mut fg);
    fg
}

fn round_bar() -> FeatureGraph {
    graph(|fg| {
        fg.stock_form = StockForm::RoundBar {
            diameter_mm: 20.0,
            length_mm: 60.0,
        }
    })
}

fn shop_quote(fg: &FeatureGraph, qty: u32, rates: &[MachineRate]) -> QuoteBreakdown {
    quote_with_shop_model(
        fg,
        &[default_material("6061-T6")],
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        qty,
        DEFAULT_TOL,
        &CalibrationTable::neutral(),
        rates,
        &[],
    )
    .expect("shop-model quote must succeed")
}

fn global_quote(fg: &FeatureGraph, qty: u32) -> QuoteBreakdown {
    quote(
        fg,
        &[default_material("6061-T6")],
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        qty,
        DEFAULT_TOL,
    )
    .expect("global quote must succeed")
}

#[test]
fn route_family_selects_by_geometry() {
    let p = default_parameters(); // bar_capacity_mm = 32.0
    use StockForm::*;
    // Prismatic block ⇒ 3-axis (identical to legacy `for_route(false)`).
    assert_eq!(
        route_family(RectangularBlock, false, 0.0, &p),
        MachineFamily::ThreeAxisMill
    );
    // The 5-axis flag wins outright, regardless of stock form.
    assert_eq!(
        route_family(RectangularBlock, true, 0.0, &p),
        MachineFamily::FiveAxisMill
    );
    assert_eq!(
        route_family(
            RoundBar {
                diameter_mm: 20.0,
                length_mm: 60.0
            },
            true,
            20.0,
            &p
        ),
        MachineFamily::FiveAxisMill
    );
    // Round/tube within bar capacity ⇒ lights-out Swiss (boundary `<=`).
    assert_eq!(
        route_family(
            RoundBar {
                diameter_mm: 20.0,
                length_mm: 60.0
            },
            false,
            20.0,
            &p
        ),
        MachineFamily::SwissTurnMill
    );
    assert_eq!(
        route_family(
            RoundBar {
                diameter_mm: 32.0,
                length_mm: 60.0
            },
            false,
            32.0,
            &p
        ),
        MachineFamily::SwissTurnMill
    );
    assert_eq!(
        route_family(
            Tube {
                od_mm: 25.0,
                id_mm: 10.0,
                length_mm: 15.0
            },
            false,
            25.0,
            &p
        ),
        MachineFamily::SwissTurnMill
    );
    // Larger round ⇒ turn-mill.
    assert_eq!(
        route_family(
            RoundBar {
                diameter_mm: 40.0,
                length_mm: 60.0
            },
            false,
            40.0,
            &p
        ),
        MachineFamily::TurnMill
    );
    assert_eq!(
        route_family(
            Tube {
                od_mm: 80.0,
                id_mm: 60.0,
                length_mm: 15.0
            },
            false,
            80.0,
            &p
        ),
        MachineFamily::TurnMill
    );
}

#[test]
fn new_machine_families_roundtrip() {
    assert_eq!(MachineFamily::ALL.len(), 11);
    for f in MachineFamily::ALL {
        assert_eq!(MachineFamily::from_db_str(f.as_db_str()), Some(f));
    }
    assert_eq!(MachineFamily::SwissTurnMill.as_db_str(), "swiss-turn-mill");
    assert_eq!(MachineFamily::TurnMill.as_db_str(), "turn-mill");
    assert_eq!(MachineFamily::FourAxisMill.as_db_str(), "4-axis-mill");
    assert_eq!(MachineFamily::from_db_str("nonsense"), None);
}

#[test]
fn empty_rates_are_byte_identical_to_global() {
    for fg in [graph(|_| {}), round_bar()] {
        let base = global_quote(&fg, DEFAULT_QTY);
        let shop = shop_quote(&fg, DEFAULT_QTY, &[]);
        assert_eq!(
            base, shop,
            "empty machine_rates must be byte-identical (incl. reasoning_log)"
        );
    }
}

#[test]
fn non_matching_rate_table_falls_back_to_global() {
    // The round bar routes to swiss-turn-mill; a table that only carries a
    // 3-axis row has no match ⇒ global rate ⇒ byte-identical, no extra line.
    let fg = round_bar();
    let base = shop_quote(&fg, DEFAULT_QTY, &[]);
    let rates = vec![MachineRate {
        family: "3-axis-mill".to_string(),
        attended_rate_eur_per_min: 9.99,
        lights_out_factor: 0.1,
        unattended_capable: true,
    }];
    let shop = shop_quote(&fg, DEFAULT_QTY, &rates);
    assert_eq!(
        base, shop,
        "no row for the routed family ⇒ global rate, byte-identical"
    );
}

#[test]
fn lights_out_effective_rate_scales_machining_cost() {
    let fg = round_bar();
    let qty = DEFAULT_QTY; // 10 >= setup_amortization_threshold (5)
    let global = shop_quote(&fg, qty, &[]);
    let rates = vec![MachineRate {
        family: "swiss-turn-mill".to_string(),
        attended_rate_eur_per_min: 1.5,
        lights_out_factor: 0.35,
        unattended_capable: true,
    }];
    let shop = shop_quote(&fg, qty, &rates);

    let global_rate = 1.6667_f64;
    let effective = 1.5 * 0.35; // 0.525
                                // Machining cost scales by exactly effective/global (the thin-wall and
                                // quote_multiplier bumps, if any, cancel in the ratio).
    let expected = global.machining_cost * (effective / global_rate);
    assert!(
        (shop.machining_cost - expected).abs() < 1e-9,
        "machining_cost should scale by effective/global; got {} expected {}",
        shop.machining_cost,
        expected
    );
    // Direct check (this fixture has no thin-wall / quote_multiplier bump and
    // Standard tolerance ⇒ multiplier 1.0, inspection 0): cost = billable*rate.
    let billable = shop.machining_minutes + shop.inspection_minutes;
    assert!((shop.machining_cost - billable * effective).abs() < 1e-9);
    // Physical minutes are unchanged — only cost-per-minute drops.
    assert_eq!(shop.machining_minutes, global.machining_minutes);
    // Setup stays on the global rate (S3 scope: setup is attended) and
    // material is untouched.
    assert_eq!(round4(shop.setup_cost), round4(global.setup_cost));
    assert_eq!(round4(shop.material_cost), round4(global.material_cost));
    // The decision is in the trust log.
    assert!(
        shop.reasoning_log
            .iter()
            .any(|l| l.contains("routed_family=swiss-turn-mill")
                && l.contains("lights_out_eligible=true")
                && l.contains("effective_rate=0.5250")),
        "reasoning_log must surface the lights-out rate decision"
    );
}

#[test]
fn below_threshold_uses_attended_not_lights_out() {
    let fg = round_bar();
    let qty = 4u32; // < threshold 5 ⇒ not lights-out-eligible
    let rates = vec![MachineRate {
        family: "swiss-turn-mill".to_string(),
        attended_rate_eur_per_min: 1.5,
        lights_out_factor: 0.35,
        unattended_capable: true,
    }];
    let shop = shop_quote(&fg, qty, &rates);
    let billable = shop.machining_minutes + shop.inspection_minutes;
    assert!(
        (shop.machining_cost - billable * 1.5).abs() < 1e-9,
        "below threshold ⇒ attended 1.5, no lights-out factor"
    );
    assert!(shop
        .reasoning_log
        .iter()
        .any(|l| l.contains("lights_out_eligible=false") && l.contains("effective_rate=1.5000")));
}

#[test]
fn attended_only_family_ignores_lights_out_factor() {
    let fg = round_bar();
    let rates = vec![MachineRate {
        family: "swiss-turn-mill".to_string(),
        attended_rate_eur_per_min: 1.5,
        lights_out_factor: 0.35,
        unattended_capable: false, // cannot run unattended
    }];
    let shop = shop_quote(&fg, DEFAULT_QTY, &rates);
    let billable = shop.machining_minutes + shop.inspection_minutes;
    assert!(
        (shop.machining_cost - billable * 1.5).abs() < 1e-9,
        "unattended_capable=false ⇒ attended rate, factor ignored"
    );
    assert!(shop
        .reasoning_log
        .iter()
        .any(|l| l.contains("lights_out_eligible=false")));
}

#[test]
fn prismatic_uses_routed_family_rate_without_lights_out() {
    // A RectangularBlock routes to 3-axis. A 3-axis rate row applies its
    // attended rate, but lights-out never triggers (not turned bar stock),
    // even though the row is flagged unattended_capable.
    let fg = graph(|_| {});
    let global = shop_quote(&fg, DEFAULT_QTY, &[]);
    let rates = vec![MachineRate {
        family: "3-axis-mill".to_string(),
        attended_rate_eur_per_min: 2.0,
        lights_out_factor: 0.5,
        unattended_capable: true,
    }];
    let shop = shop_quote(&fg, DEFAULT_QTY, &rates);
    let billable = shop.machining_minutes + shop.inspection_minutes;
    assert!((shop.machining_cost - billable * 2.0).abs() < 1e-9);
    assert!(shop.reasoning_log.iter().any(
        |l| l.contains("routed_family=3-axis-mill") && l.contains("lights_out_eligible=false")
    ));
    assert!(
        (shop.machining_cost - global.machining_cost).abs() > 1e-9,
        "a 3-axis rate of 2.0 must differ from the global 1.6667"
    );
}
