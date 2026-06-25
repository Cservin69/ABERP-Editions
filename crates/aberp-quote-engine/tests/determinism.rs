//! Determinism: same inputs ⇒ byte-identical `reasoning_log`. This
//! is the contract that makes the reasoning_log a trust signal per
//! `[[trust-code-not-operator]]`.

mod common;

use aberp_quote_engine::quote;
use common::*;

#[test]
fn reasoning_log_byte_identical_across_runs() {
    let materials = vec![default_material("6061-T6"), exotic_material("Inconel 718")];
    let rules = catchall_complexity_rules();
    let tols = default_tolerance_multipliers();
    let adjs = no_stock_adjustments();
    let params = default_parameters();
    let fg = simple_feature_graph("6061-T6");

    let first = quote(
        &fg,
        &materials,
        &rules,
        &tols,
        &adjs,
        &params,
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .unwrap();
    let second = quote(
        &fg,
        &materials,
        &rules,
        &tols,
        &adjs,
        &params,
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .unwrap();

    assert_eq!(
        first.reasoning_log, second.reasoning_log,
        "reasoning_log must be byte-identical across identical-input runs",
    );
    assert_eq!(first, second, "whole breakdown must be byte-identical");
}

#[test]
fn reasoning_log_byte_identical_with_reordered_complexity_rules() {
    // The engine MUST be order-insensitive on the catalogue input
    // (it picks the most-specific rule deterministically). If the
    // input slice reorders, the picked rule and the log lines are
    // unchanged.
    let materials = vec![default_material("6061-T6")];
    let mut rules_a = catchall_complexity_rules();
    let mut rules_b = rules_a.clone();
    rules_b.reverse();
    // Reversing doesn't change picks because each (ft, sb) has one
    // rule. Stronger test: introduce two rules for the same triple
    // and assert tightest-range wins regardless of input order.
    rules_a.push(aberp_quote_engine::ComplexityRule {
        id: 9001,
        feature_type: "hole".to_string(),
        size_bucket: "XS".to_string(),
        count_min: 1,
        count_max: Some(10),
        base_time_minutes: 1.0,
        multiplier: 0.5,
        setup_penalty_minutes: 1.0,
    });
    rules_b = rules_a.clone();
    rules_b.reverse();

    let tols = default_tolerance_multipliers();
    let adjs = no_stock_adjustments();
    let params = default_parameters();
    let fg = simple_feature_graph("6061-T6");

    let from_a = quote(
        &fg,
        &materials,
        &rules_a,
        &tols,
        &adjs,
        &params,
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .unwrap();
    let from_b = quote(
        &fg,
        &materials,
        &rules_b,
        &tols,
        &adjs,
        &params,
        DEFAULT_QTY,
        DEFAULT_TOL,
    )
    .unwrap();
    assert_eq!(
        from_a.reasoning_log, from_b.reasoning_log,
        "reasoning_log must be order-insensitive on the input slice",
    );
    assert_eq!(from_a.total_price, from_b.total_price);
}

#[test]
fn machine_family_vocabulary_is_stable() {
    // ADR-0094 Gap 2 grew the closed vocab 8 → 11 by APPENDING (never
    // reordering), so every pre-existing db-string and discriminant — which
    // the engine uses as a `BTreeMap` key + lead-time tie-break — is
    // unchanged. This pins the order so a future reorder fails loudly.
    use aberp_quote_engine::MachineFamily;
    let expected: [&str; 11] = [
        "3-axis-mill",
        "5-axis-mill",
        "wire-EDM",
        "sinker-EDM",
        "lathe",
        "grinder",
        "additive",
        "other",
        "swiss-turn-mill",
        "turn-mill",
        "4-axis-mill",
    ];
    let got: Vec<&str> = MachineFamily::ALL.iter().map(|f| f.as_db_str()).collect();
    assert_eq!(
        got, expected,
        "MachineFamily::ALL order + db-strings must be stable"
    );
    for f in MachineFamily::ALL {
        assert_eq!(MachineFamily::from_db_str(f.as_db_str()), Some(f));
    }
}

#[test]
fn shop_model_reasoning_log_byte_identical_across_runs() {
    use aberp_quote_engine::{quote_with_shop_model, CalibrationTable, MachineRate, StockForm};
    let materials = vec![default_material("6061-T6")];
    let rules = catchall_complexity_rules();
    let tols = default_tolerance_multipliers();
    let adjs = no_stock_adjustments();
    let params = default_parameters();
    let mut fg = simple_feature_graph("6061-T6");
    fg.stock_form = StockForm::RoundBar {
        diameter_mm: 18.0,
        length_mm: 55.0,
    };
    let rates = vec![
        MachineRate {
            family: "swiss-turn-mill".to_string(),
            attended_rate_eur_per_min: 1.5,
            lights_out_factor: 0.35,
            unattended_capable: true,
        },
        MachineRate {
            family: "turn-mill".to_string(),
            attended_rate_eur_per_min: 1.6,
            lights_out_factor: 0.45,
            unattended_capable: true,
        },
    ];
    let run = || {
        quote_with_shop_model(
            &fg,
            &materials,
            &rules,
            &tols,
            &adjs,
            &params,
            DEFAULT_QTY,
            DEFAULT_TOL,
            &CalibrationTable::neutral(),
            &rates,
        )
        .unwrap()
    };
    let a = run();
    let b = run();
    assert_eq!(
        a, b,
        "shop-model quote must be byte-identical across identical-input runs"
    );
    assert!(a
        .reasoning_log
        .iter()
        .any(|l| l.contains("effective_rate=0.5250")));
}
