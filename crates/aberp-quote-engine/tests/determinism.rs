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
