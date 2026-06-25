//! S418 benchmark band — the model must land a real, machined part in
//! the calibrated price range (report §7.2). The pre-S418 engine quoted
//! this PEEK cube material-only (machining 0.00, no CAD-CAM) at ~€47;
//! the geometry model brings it to ~€73/part. This test pins that the
//! whole model is wired — every cost line is non-zero — and the total
//! sits in the benchmark band. A regression that re-zeros machining or
//! CAD-CAM (the exact pre-S418 bug) fails HERE, loud (CLAUDE.md #9).

mod common;

use aberp_quote_engine::{
    quote, Feature, FeatureGraph, Material, StockForm, StockStatus, ToleranceRange,
};
use common::*;

/// PEEK, 15 pcs, solid 50 mm cube, Standard tolerance — the report
/// §7.2 anchor (≈ €73/part). Surface area is the true cube area
/// (6·50² = 15000 mm²) so this exercises the real v2 path, not the
/// bbox fallback (for a solid cube they coincide).
#[test]
fn peek_15pcs_50mm_cube_lands_in_benchmark_band() {
    let peek = Material {
        grade: "PEEK".to_string(),
        density_g_cm3: 1.30,
        cost_per_kg_eur: 90.0,
        machining_difficulty: 0.8,
        quote_multiplier: 1.0,
        stock_status: StockStatus::Source1_2d,
    };
    let fg = FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        bounding_box_mm: [50.0, 50.0, 50.0],
        volume_mm3: 125_000.0,
        surface_area_mm2: 15_000.0,
        material_grade: "PEEK".to_string(),
        features: Vec::<Feature>::new(),
        requires_5_axis: false,
        thin_wall_present: false,
        stock_form: StockForm::RectangularBlock,
        gears: Vec::new(),
    };

    let r = quote(
        &fg,
        &[peek],
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        15,
        ToleranceRange::Standard,
    )
    .expect("PEEK benchmark quote must succeed");

    // Every line is wired — the pre-S418 bug was machining_cost == 0
    // and no CAD-CAM line at all.
    assert!(r.material_cost > 0.0, "material_cost must be > 0");
    assert!(
        r.machining_cost > 0.0,
        "machining_cost must be > 0 (the pre-S418 bug was 0.00)"
    );
    assert!(r.cad_cam_cost > 0.0, "cad_cam_cost must be > 0");
    assert!(r.setup_cost > 0.0, "setup_cost must be > 0");
    assert!(r.machining_minutes > 0.0, "machining_minutes must be > 0");

    // Benchmark band: report §7.2 calibrates this at ≈ €73/part. The
    // exact model output is ≈ €72.63; the band catches a model
    // regression while tolerating a knob tweak within the calibration.
    assert!(
        (70.0..=75.0).contains(&r.total_price),
        "PEEK 15pcs total {:.4} EUR outside benchmark band [70, 75] (expected ≈ 72.63)",
        r.total_price
    );

    // Margin floor still honoured.
    assert!(r.margin / r.total_price >= 0.10);
}
