//! S1 / ADR-0094 Gap 1 — new goldens for the non-default stock forms.
//!
//! The inert-by-default mechanism (serde defaults + `RectangularBlock`)
//! protects *old* prices (see `golden.rs`/`determinism.rs`). THESE
//! fixtures protect the *new* math: a round bar bills `π/4·d²·L·(1+scrap)`
//! (~21.5 % less material than its bounding-box block, with proportionally
//! less roughing), and a tube excludes its bore from BOTH the billed
//! material AND the roughing removed-volume. Hand-derived to 4 dp; the
//! arithmetic chain is in each test's comment.

mod common;

use aberp_quote_engine::{quote, FeatureGraph, StockForm, ToleranceRange, ToleranceSpec};
use common::*;

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// A turned/tubular blank: no milled features (so the derivation isolates
/// material + roughing + finishing), `6061-T6`, Standard tol, qty 10.
fn blank(
    stock_form: StockForm,
    bbox: [f64; 3],
    volume_mm3: f64,
    surface_area_mm2: f64,
) -> FeatureGraph {
    FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        tolerance: ToleranceSpec::Unspecified,
        critical_feature_tolerances: Vec::new(),
        bounding_box_mm: bbox,
        volume_mm3,
        surface_area_mm2,
        material_grade: "6061-T6".to_string(),
        features: vec![],
        requires_5_axis: false,
        thin_wall_present: false,
        stock_form,
        gears: Vec::new(),
    }
}

fn quote_blank(fg: &FeatureGraph) -> aberp_quote_engine::QuoteBreakdown {
    let materials = vec![default_material("6061-T6")];
    quote(
        fg,
        &materials,
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &default_parameters(),
        10,
        ToleranceRange::Standard,
    )
    .expect("stock-form blank quote must succeed")
}

#[test]
fn round_bar_sun_blank_locked_4dp() {
    // Ø40 × L30 sun-gear blank; bbox = [40,40,30], part vol 30000 mm³,
    // surface 5000 mm² (real value → no bbox fallback).
    //   form_volume = π/4·40²·30           = 37699.1118 mm³
    //   stock       = 37699.1118 · 1.15    = 43353.9786
    //   mass_kg     = 43353.9786·2.70/1e6  = 0.11705574
    //   material    = ·8.0                 = 0.9364 EUR
    //   removed_cm3 = (43353.9786-30000)/1e3= 13.353979
    //   roughing    = 13.353979·1.0/8.0    = 1.669247 min
    //   finishing   = (5000/100)·0.08·1.0  = 4.0 min
    //   machining_min = 1.669247 + 4.0     = 5.6692
    //   machining_cost= 5.6692·1.6667·1.0  = 9.4489 EUR
    //   setup = 20·1.6667/10 = 3.3334; cad_cam: fill 0.625 ⇒ base 1.0 ⇒ 10.0
    //   subtotal 23.7188; overhead 4.7438; margin 9.9619; total 38.4244
    let r = quote_blank(&blank(
        StockForm::RoundBar {
            diameter_mm: 40.0,
            length_mm: 30.0,
        },
        [40.0, 40.0, 30.0],
        30_000.0,
        5_000.0,
    ));
    assert_eq!(round4(r.material_cost), 0.9364, "material_cost");
    assert_eq!(round4(r.machining_minutes), 5.6692, "machining_minutes");
    assert_eq!(round4(r.machining_cost), 9.4489, "machining_cost");
    assert_eq!(round4(r.setup_cost), 3.3334, "setup_cost");
    assert_eq!(round4(r.cad_cam_cost), 10.0000, "cad_cam_cost");
    assert_eq!(round4(r.overhead), 4.7438, "overhead");
    assert_eq!(round4(r.margin), 9.9619, "margin");
    assert_eq!(round4(r.total_price), 38.4244, "total_price");

    // Reasoning log names the form and the formula (the two new lines).
    assert!(
        r.reasoning_log.iter().any(|l| l.contains("stock_form=round_bar")
            && l.contains("form_volume_mm3=37699.1118")),
        "round_bar form line missing: {:#?}",
        r.reasoning_log
    );
    assert!(
        r.reasoning_log
            .iter()
            .any(|l| l.contains("form_volume 37699.1118") && l.contains("= 43353.9786")),
        "round_bar stock_volume line missing"
    );
}

#[test]
fn round_bar_bills_pi_over_4_of_the_block() {
    // Same bbox/part/surface; flip only the stock form. The round bar must
    // bill EXACTLY π/4 of the rectangular block's material (≈ 21.46 % less)
    // and rough less (a near-net bar removes only what was bought).
    let bbox = [40.0, 40.0, 30.0];
    let round = quote_blank(&blank(
        StockForm::RoundBar {
            diameter_mm: 40.0,
            length_mm: 30.0,
        },
        bbox,
        30_000.0,
        5_000.0,
    ));
    let block = quote_blank(&blank(StockForm::RectangularBlock, bbox, 30_000.0, 5_000.0));

    let ratio = round.material_cost / block.material_cost;
    assert!(
        (ratio - std::f64::consts::FRAC_PI_4).abs() < 1e-9,
        "round/block material must be π/4 (got {ratio})"
    );
    assert!(
        (1.0 - ratio) > 0.2140 && (1.0 - ratio) < 0.2150,
        "round bar bills ~21.46% less material (got {:.4}%)",
        (1.0 - ratio) * 100.0
    );
    assert!(
        round.machining_minutes < block.machining_minutes,
        "near-net bar must rough less than the block ({} vs {})",
        round.machining_minutes,
        block.machining_minutes
    );
}

#[test]
fn tube_ring_blank_locked_4dp() {
    // Ø100/Ø80 × L15 ring-gear blank; bbox = [100,100,15], part vol
    // 40000 mm³, surface 8000 mm².
    //   form_volume = π/4·(100²-80²)·15    = 42411.5008 mm³ (annulus)
    //   stock       = ·1.15                = 48773.2259
    //   material    = 48773.2259·2.70/1e6·8= 1.0535 EUR
    //   removed_cm3 = (48773.2259-40000)/1e3= 8.773226
    //   roughing    = 8.773226/8.0         = 1.096653 min
    //   finishing   = (8000/100)·0.08      = 6.4 min
    //   machining_min = 7.4967; machining_cost = 7.4967·1.6667 = 12.4947
    //   setup 3.3334; fill 40000/150000=0.2667 < 0.30 ⇒ low_fill ⇒
    //   cad_cam_hours 2.0 ⇒ cad_cam 20.0
    //   subtotal 36.8816; overhead 7.3763; margin 15.4903; total 59.7481
    let r = quote_blank(&blank(
        StockForm::Tube {
            od_mm: 100.0,
            id_mm: 80.0,
            length_mm: 15.0,
        },
        [100.0, 100.0, 15.0],
        40_000.0,
        8_000.0,
    ));
    assert_eq!(round4(r.material_cost), 1.0535, "material_cost");
    assert_eq!(round4(r.machining_minutes), 7.4967, "machining_minutes");
    assert_eq!(round4(r.machining_cost), 12.4947, "machining_cost");
    assert_eq!(round4(r.setup_cost), 3.3334, "setup_cost");
    assert_eq!(round4(r.cad_cam_cost), 20.0000, "cad_cam_cost");
    assert_eq!(round4(r.overhead), 7.3763, "overhead");
    assert_eq!(round4(r.margin), 15.4903, "margin");
    assert_eq!(round4(r.total_price), 59.7481, "total_price");

    assert!(
        r.reasoning_log.iter().any(|l| l.contains("stock_form=tube")
            && l.contains("bore not bought")
            && l.contains("form_volume_mm3=42411.5008")),
        "tube form line missing: {:#?}",
        r.reasoning_log
    );
}

#[test]
fn tube_excludes_bore_from_material_and_roughing() {
    // Same bbox/part/surface; compare tube vs a SOLID bar of the same OD
    // vs the block. The bore (id) must be billed by NEITHER material NOR
    // roughing: tube material = (od²-id²)/od² = 0.36 of the solid bar.
    let bbox = [100.0, 100.0, 15.0];
    let tube = quote_blank(&blank(
        StockForm::Tube {
            od_mm: 100.0,
            id_mm: 80.0,
            length_mm: 15.0,
        },
        bbox,
        40_000.0,
        8_000.0,
    ));
    let solid = quote_blank(&blank(
        StockForm::RoundBar {
            diameter_mm: 100.0,
            length_mm: 15.0,
        },
        bbox,
        40_000.0,
        8_000.0,
    ));
    let block = quote_blank(&blank(StockForm::RectangularBlock, bbox, 40_000.0, 8_000.0));

    let annulus_ratio = tube.material_cost / solid.material_cost;
    assert!(
        (annulus_ratio - 0.36).abs() < 1e-9,
        "tube bills only the annulus: (100²-80²)/100² = 0.36 (got {annulus_ratio})"
    );
    assert!(
        tube.material_cost < solid.material_cost && solid.material_cost < block.material_cost,
        "tube < solid-OD bar < block on material"
    );
    // Bore excluded from roughing too: the block model would "rough away"
    // the entire bore + corners; the tube removes far less.
    assert!(
        tube.machining_minutes < block.machining_minutes,
        "tube roughing must exclude the bore ({} vs block {})",
        tube.machining_minutes,
        block.machining_minutes
    );
}
