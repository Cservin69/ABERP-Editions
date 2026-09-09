//! ADR-0112 Part C / D-19 slice C — the drilling cycle-time cost model.
//!
//! These pin the NEW located-holes drilling path. The base part is
//! `simple_feature_graph("6061-T6")` (which carries a `Hole` feature, count 4)
//! + `default_*` fixtures, qty `DEFAULT_QTY`, band `DEFAULT_TOL`.
//!
//! The load-bearing invariants:
//! - **inert = byte-identical.** A graph that merely *carries* `located_holes`
//!   with no rate slice / no matching row / an inert `feed <= 0` row prices
//!   exactly as one with no holes at all, and logs no `[drilling]` line. This
//!   is the Portable-never-moves / seeded-zero-contribution contract.
//! - **active = priced + logged**, per the §C.2 formula.
//! - **double-count guard**: an active drilling model supersedes the `Hole`
//!   feature's machining minutes (located geometry wins over counted).

mod common;

use aberp_quote_engine::{
    quote_with_catalogue, CalibrationTable, DrillingRate, HoleEndCondition, LocatedHole,
    QuoteBreakdown,
};
use common::*;

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// A fully-seeded rate for the 6061 group. Chosen for clean hand-arithmetic.
fn active_rate() -> DrillingRate {
    DrillingRate {
        material_group: "6061-T6".to_string(),
        feed_mm_per_min_per_mm_dia: 100.0,
        peck_depth_dia_multiple: 3.0,
        peck_retract_sec: 2.0,
        rapid_per_hole_sec: 3.0,
        tool_change_sec: 6.0,
        flat_bottom_factor: 1.5,
        unknown_end_condition_factor: 1.2,
    }
}

/// The zero-contribution seed sentinel: a row exists but `feed = 0` ⇒ inert.
fn inert_rate() -> DrillingRate {
    DrillingRate {
        feed_mm_per_min_per_mm_dia: 0.0,
        ..active_rate()
    }
}

fn hole(d: f64, l: f64, end: HoleEndCondition, flat: bool) -> LocatedHole {
    LocatedHole {
        diameter_mm: d,
        depth_mm: l,
        axis_unit: [0.0, 0.0, -1.0],
        entry_point_mm: [0.0, 0.0, 0.0],
        end_condition: end,
        flat_bottom: flat,
    }
}

fn quote_drill(holes: Vec<LocatedHole>, rates: Vec<DrillingRate>) -> QuoteBreakdown {
    let mut fg = simple_feature_graph("6061-T6");
    fg.located_holes = holes;
    let mut fx = CatalogueFixture::new("6061-T6");
    fx.drilling_rates = rates;
    quote_with_catalogue(
        &fg,
        &fx.snapshot(),
        &default_parameters(),
        DEFAULT_QTY,
        DEFAULT_TOL,
        &CalibrationTable::neutral(),
    )
    .expect("drilling quote must succeed")
}

fn baseline() -> QuoteBreakdown {
    quote_drill(vec![], vec![])
}

fn has_drilling_line(bd: &QuoteBreakdown) -> bool {
    bd.reasoning_log.iter().any(|l| l.contains("[drilling]"))
}

// ── Inert paths — byte-identical, no [drilling] line ─────────────────────

#[test]
fn holes_with_no_rate_slice_are_byte_identical() {
    let base = baseline();
    let with_holes = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Through, false)],
        vec![],
    );
    assert_eq!(with_holes.total_price, base.total_price);
    assert_eq!(with_holes.machining_minutes, base.machining_minutes);
    assert!(
        !has_drilling_line(&with_holes),
        "no rate slice ⇒ no [drilling] line"
    );
}

#[test]
fn an_inert_feed_zero_row_is_byte_identical() {
    let base = baseline();
    let seeded = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Through, false)],
        vec![inert_rate()],
    );
    assert_eq!(seeded.total_price, base.total_price);
    assert_eq!(seeded.machining_minutes, base.machining_minutes);
    assert!(
        !has_drilling_line(&seeded),
        "feed<=0 seed ⇒ inert, no [drilling] line"
    );
}

#[test]
fn no_rate_for_the_material_warns_and_stays_zero() {
    // A rate exists, but for a different group — no match for 6061-T6.
    let ti = DrillingRate {
        material_group: "Ti-6Al-4V".to_string(),
        ..active_rate()
    };
    let base = baseline();
    let bd = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Through, false)],
        vec![ti],
    );
    // No matching rate ⇒ drilling inert (0 minutes) AND the Hole feature is
    // charged as before ⇒ byte-identical price — but a loud WARNING is logged.
    assert_eq!(bd.total_price, base.total_price);
    assert!(
        bd.reasoning_log.iter().any(
            |l| l.contains("[drilling] WARNING no DrillingRate row for material_group=6061-T6")
        ),
        "a holes-but-no-rate part must warn: {:?}",
        bd.reasoning_log
    );
}

// ── Active path — priced + logged ────────────────────────────────────────

#[test]
fn a_through_hole_is_priced_by_the_c2_formula() {
    // d=6, L=30, through, 6061-T6 difficulty 1.0, active_rate:
    //   cut   = 30 / (100*6) * 1.0        = 0.05
    //   peck  = (ceil(30/(3*6)) - 1) * 2/60 = (2-1)*2/60 = 0.033333
    //   rapid = 3/60                      = 0.05
    //   hole  = (0.05+0.033333+0.05)*1.0  = 0.133333
    //   tool  = 1 distinct dia * 6/60     = 0.1
    //   drilling_minutes                  = 0.233333
    let bd = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Through, false)],
        vec![active_rate()],
    );
    assert!(has_drilling_line(&bd));
    // Per-hole: (0.05 + 0.033333 + 0.05) * 1.0 = 0.1333.
    assert!(
        bd.reasoning_log
            .iter()
            .any(|l| l.contains("hole#0") && l.contains("= 0.1333 min")),
        "expected per-hole 0.1333: {:?}",
        bd.reasoning_log
    );
    // Summary: 0.1333 + tool_change 0.1 = 0.2333.
    assert!(
        bd.reasoning_log
            .iter()
            .any(|l| l.contains("drilling_minutes 0.2333 min")),
        "expected drilling_minutes 0.2333: {:?}",
        bd.reasoning_log
    );
    // (`machining_minutes` vs a bare part is NOT a clean comparison here: the
    // double-count guard removes the base graph's 4-count Hole *feature*
    // minutes and prices the single located hole instead, so the total can move
    // either way — the formula value logged above is the real pin.)
}

#[test]
fn distinct_diameters_drive_tool_changes() {
    let same = quote_drill(
        vec![
            hole(6.0, 30.0, HoleEndCondition::Through, false),
            hole(6.0, 30.0, HoleEndCondition::Through, false),
        ],
        vec![active_rate()],
    );
    let diff = quote_drill(
        vec![
            hole(6.0, 30.0, HoleEndCondition::Through, false),
            hole(8.0, 30.0, HoleEndCondition::Through, false),
        ],
        vec![active_rate()],
    );
    assert!(same
        .reasoning_log
        .iter()
        .any(|l| l.contains("1 distinct diameters")));
    assert!(diff
        .reasoning_log
        .iter()
        .any(|l| l.contains("2 distinct diameters")));
    // Two tool changes cost more machine time than one (same cut work: the 8 mm
    // hole is faster to cut, so the extra tool change is the dominant delta —
    // assert the tool-change count, above, rather than a fragile minute compare).
}

#[test]
fn unknown_end_condition_is_conservative_and_flat_bottom_is_slower() {
    let through = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Through, false)],
        vec![active_rate()],
    );
    let unknown = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Unknown, false)],
        vec![active_rate()],
    );
    let flat = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Blind, true)],
        vec![active_rate()],
    );
    // end_factor: Through 1.0 < Unknown 1.2 < flat-bottom 1.5.
    assert!(unknown.machining_minutes > through.machining_minutes);
    assert!(flat.machining_minutes > unknown.machining_minutes);
}

#[test]
fn peck_count_scales_with_depth() {
    // Shallow: L=10, peck depth 18 ⇒ ceil(10/18)-1 = 0 pecks.
    let shallow = quote_drill(
        vec![hole(6.0, 10.0, HoleEndCondition::Through, false)],
        vec![active_rate()],
    );
    // Deep: L=60 ⇒ ceil(60/18)-1 = 4-1 = 3 pecks.
    let deep = quote_drill(
        vec![hole(6.0, 60.0, HoleEndCondition::Through, false)],
        vec![active_rate()],
    );
    assert!(shallow.reasoning_log.iter().any(|l| l.contains("peck×0")));
    assert!(deep.reasoning_log.iter().any(|l| l.contains("peck×3")));
    assert!(round4(deep.machining_minutes) > round4(shallow.machining_minutes));
}

// ── Double-count guard ───────────────────────────────────────────────────

#[test]
fn an_active_drilling_model_supersedes_the_hole_feature() {
    // The base graph carries a Hole feature (count 4). With an active drilling
    // model, those hole machining minutes are priced per-hole instead — the
    // feature contributes setup/complexity but not machining minutes.
    let bd = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Through, false)],
        vec![active_rate()],
    );
    assert!(
        bd.reasoning_log.iter().any(|l| l
            .contains("machining minutes SUPERSEDED by the located-holes drilling model")),
        "the double-count guard must fire under an active drilling model: {:?}",
        bd.reasoning_log
    );
}

#[test]
fn an_inert_drilling_model_does_not_supersede_the_hole_feature() {
    // With no active rate, the guard must stay off so the Hole feature is still
    // charged — otherwise a Portable part carrying located_holes would silently
    // drop its hole minutes.
    let bd = quote_drill(
        vec![hole(6.0, 30.0, HoleEndCondition::Through, false)],
        vec![inert_rate()],
    );
    assert!(
        !bd.reasoning_log.iter().any(|l| l.contains("SUPERSEDED")),
        "an inert drilling model must NOT supersede the hole feature"
    );
}
