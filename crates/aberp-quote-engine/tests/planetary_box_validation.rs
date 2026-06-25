//! S7 / ADR-0094 — **validation golden**: a Ø100 compound planetary gearbox,
//! qty 100 boxes, 6061-T6, priced through the gap-closed engine
//! ([`quote_with_shop_model`]). This is the integration proof that the three
//! gaps (S1–S2 stock-form, S3–S4 machine-family rates + lights-out, S5–S6
//! gear-generation ops) compose into a realistic, fully-decomposable quote.
//!
//! ## What this fixture proves (direction + decomposition, NOT an exact € target)
//!
//! Per ADR-0094's adversarial review and the gap-closure plan (Risk R1), the
//! external **€444/box** baseline Ervin ran is NOT in this tree — it is a
//! manual figure that *included* a flat gear adder (~€95/box; ADR-0094
//! Context). So the validation target is framed as **direction + every line
//! present and individually inspectable in the reasoning_log**, anchored by a
//! naive in-tree baseline this test also computes.
//!
//! ## ⚠️ ILLUSTRATIVE GEAR/GEOMETRY DATA — RECHECK WITH REAL GEAR DATA
//!
//! The tooth counts, modules, face widths and AGMA classes below are
//! **Dispatch-supplied illustrative geometry**, NOT measured from a real
//! drawing. The seed machine rates (ADR-0094 Gap 2 proposed €/min) and gear-
//! process coefficients are likewise illustrative, chosen partly for clean
//! arithmetic. They **self-correct** via the S429 calibration loop (actual ÷
//! estimated per family) once Ervin's real shop rates and a real gear drawing
//! are entered. **Before quoting a real planetary set, recheck every gear
//! parameter and every seed rate against the customer's drawing + the shop's
//! measured €/min.**
//!
//! ## ⚠️ HONEST BASELINE FLAG (conservative call, see report)
//!
//! The brief's literal assertion — "per-box total LOWER than a naive
//! all-3-axis / no-gear / rectangular-block baseline" — **cannot hold for a
//! gear-heavy assembly without dishonesty**, and this test does not pretend it
//! does. Reason: the naive baseline OMITS gear cost entirely (the pre-Gap-3
//! engine has no gear concept), whereas the gap-closed engine correctly bills
//! ~€133/box of real tooth-generation work. Surfacing that hidden cost is the
//! whole point of Gap 3 — it legitimately RAISES the geared-part price above a
//! model that ignored teeth. So the strict no-gear baseline comes out *lower*
//! than the upgraded box, and we assert + document that relationship honestly
//! rather than rig inputs to invert it.
//!
//! What IS true, meaningful, and asserted below:
//!   * the upgraded per-box total is finite, > 0, and **below the €444 external
//!     baseline** and **below the realistic pre-gap quote** (naive block
//!     baseline + the documented legacy flat gear adder);
//!   * the **non-geared turned parts** (pins, hub) — where the gaps are purely
//!     cost-reducing — are strictly cheaper upgraded than naive (gaps 1+2);
//!   * the **geared parts** carry an explicit, decomposed `gear_cost > 0` the
//!     naive model never saw (gap 3), external skiving ≪ internal shaping;
//!   * every round/tube part bills **less material** than its bbox block (gap 1);
//!   * every turned part within bar capacity routes to a **lights-out Swiss/
//!     turn-mill effective €/min**, not the flat €100/h (gap 2).
//!
//! ## Pinned golden (per-component + per-box €), recorded 2026-06-25
//!
//! Engine `quote_with_shop_model`, qty = per-box count × 100, Standard
//! tolerance, neutral calibration, bbox-area finishing fallback
//! (`surface_area_mm2 = 0`, documented choice — a cylinder estimate is left to
//! the S269 extractor). Numbers are locked to 4 dp below and cross-checked
//! against an independent reference implementation of the engine arithmetic.

mod common;

use aberp_quote_engine::{
    quote, quote_with_shop_model, CalibrationTable, Feature, FeatureGraph, GearKind, GearOp,
    GearProcess, GearProcessRate, MachineRate, Material, QuoteBreakdown, QuotingParameters,
    StockForm, StockStatus, ToleranceRange,
};
use common::{catchall_complexity_rules, default_tolerance_multipliers, no_stock_adjustments};

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

// ── Brief catalogue: 6061-T6 @ €6/kg, density 2.7, difficulty 1.0 ──────────
fn aluminium_6061() -> Material {
    Material {
        grade: "6061-T6".to_string(),
        density_g_cm3: 2.7,
        cost_per_kg_eur: 6.0,
        machining_difficulty: 1.0,
        quote_multiplier: 1.0,
        stock_status: StockStatus::InStock,
    }
}

// ── Brief params: scrap 0.15, overhead 0.20, margin 0.35, bar_capacity 32,
//    global rate 1.6667 (= €100/machine-hour), setup-amortization qty 5. ────
fn params() -> QuotingParameters {
    QuotingParameters {
        scrap_factor: 0.15,
        profit_margin_base: 0.35,
        overhead_factor: 0.20,
        setup_amortization_threshold: 5,
        min_margin: 0.10,
        exotic_material_tax: 0.05,
        machining_rate_eur_per_minute: 1.6667,
        cad_cam_rate_eur_per_hour: 100.0,
        cad_cam_base_hours: 1.0,
        mrr_rough_ref_cm3_per_min: 8.0,
        t_finish_min_per_cm2: 0.08,
        setup_base_min: 20.0,
        setup_5axis_min: 25.0,
        bar_capacity_mm: 32.0,
    }
}

// ── Seeded shop: 6 machine families (ADR-0094 Gap 2 proposed €/min — seed,
//    illustrative, S429-calibratable). Swiss + turn-mill + lathe run lights-
//    out; mills are attended. ─────────────────────────────────────────────
fn machine_rates() -> Vec<MachineRate> {
    let r = |fam: &str, att, lof, unat| MachineRate {
        family: fam.to_string(),
        attended_rate_eur_per_min: att,
        lights_out_factor: lof,
        unattended_capable: unat,
    };
    vec![
        r("3-axis-mill", 1.6667, 1.0, false),
        r("4-axis-mill", 1.90, 1.0, false),
        r("5-axis-mill", 2.50, 1.0, false),
        r("swiss-turn-mill", 1.50, 0.35, true),
        r("turn-mill", 1.60, 0.45, true),
        r("lathe", 1.50, 0.40, true),
    ]
}

// ── Seeded shop: 5 gear processes (illustrative coefficients; PowerSkive
//    in-cycle is cheap, WireEDM is the premium). Operator-tunable catalogue. ─
fn gear_rates() -> Vec<GearProcessRate> {
    let r = |p: &str, setup, mpt, mexp, agma, icf| GearProcessRate {
        process: p.to_string(),
        setup_min: setup,
        min_per_tooth: mpt,
        module_exponent: mexp,
        agma_quality_factor_base: agma,
        in_cycle_factor: icf,
    };
    vec![
        r("hob", 20.0, 0.30, 1.0, 0.10, 1.0),
        r("power_skive", 8.0, 0.10, 1.0, 0.10, 0.5),
        r("shape", 30.0, 0.50, 1.0, 0.15, 1.0),
        r("broach", 60.0, 0.05, 1.0, 0.10, 1.0),
        r("wire_edm", 15.0, 2.00, 1.0, 0.20, 1.0),
    ]
}

fn ext_gear(module: f64, teeth: u32, face: f64, agma: u8) -> GearOp {
    GearOp {
        kind: GearKind::ExternalSpurHelical,
        module_mm: module,
        teeth,
        face_width_mm: face,
        quality_agma: agma,
        process: GearProcess::Auto,
    }
}
fn int_gear(module: f64, teeth: u32, face: f64, agma: u8) -> GearOp {
    GearOp {
        kind: GearKind::InternalRing,
        module_mm: module,
        teeth,
        face_width_mm: face,
        quality_agma: agma,
        process: GearProcess::Auto,
    }
}

/// One machined component of the box.
struct Component {
    name: &'static str,
    per_box_count: u32,
    /// Engine quantity = per-box count × 100 boxes.
    qty: u32,
    bbox: [f64; 3],
    volume_mm3: f64,
    form: StockForm,
    gears: Vec<GearOp>,
}

impl Component {
    /// The gap-closed graph: real stock form + gears + bbox-area finishing
    /// fallback (`surface_area_mm2 = 0`).
    fn upgraded_graph(&self) -> FeatureGraph {
        FeatureGraph {
            schema_version: FeatureGraph::SCHEMA_VERSION,
            bounding_box_mm: self.bbox,
            volume_mm3: self.volume_mm3,
            surface_area_mm2: 0.0,
            material_grade: "6061-T6".to_string(),
            features: Vec::<Feature>::new(),
            requires_5_axis: false,
            thin_wall_present: false,
            stock_form: self.form,
            gears: self.gears.clone(),
        }
    }
    /// The naive pre-gap graph: rectangular block (the bbox), NO gears.
    fn naive_graph(&self) -> FeatureGraph {
        FeatureGraph {
            stock_form: StockForm::RectangularBlock,
            gears: Vec::new(),
            ..self.upgraded_graph()
        }
    }
    fn upgraded(&self) -> QuoteBreakdown {
        quote_with_shop_model(
            &self.upgraded_graph(),
            &[aluminium_6061()],
            &catchall_complexity_rules(),
            &default_tolerance_multipliers(),
            &no_stock_adjustments(),
            &params(),
            self.qty,
            ToleranceRange::Standard,
            &CalibrationTable::neutral(),
            &machine_rates(),
            &gear_rates(),
        )
        .expect("upgraded planetary component quote must succeed")
    }
    /// Naive baseline = today's legacy engine: global flat rate, block, no gears.
    fn naive(&self) -> QuoteBreakdown {
        quote(
            &self.naive_graph(),
            &[aluminium_6061()],
            &catchall_complexity_rules(),
            &default_tolerance_multipliers(),
            &no_stock_adjustments(),
            &params(),
            self.qty,
            ToleranceRange::Standard,
        )
        .expect("naive baseline quote must succeed")
    }
}

fn the_box() -> Vec<Component> {
    vec![
        // Ring ×1 — solid Ø100×20 bar (OD 100 > bar cap 32 ⇒ turn-mill);
        // external Z50 + internal Z60 ring teeth (illustrative).
        Component {
            name: "ring",
            per_box_count: 1,
            qty: 100,
            bbox: [100.0, 100.0, 20.0],
            volume_mm3: 61_500.0,
            form: StockForm::RoundBar {
                diameter_mm: 100.0,
                length_mm: 20.0,
            },
            gears: vec![ext_gear(2.0, 50, 18.0, 8), int_gear(2.0, 60, 18.0, 8)],
        },
        // Planet ×5 — Ø28×18 bar, bore 10 (OD 28 ≤ 32 ⇒ Swiss lights-out);
        // external Z24 (illustrative).
        Component {
            name: "planet",
            per_box_count: 5,
            qty: 500,
            bbox: [28.0, 28.0, 18.0],
            volume_mm3: 9_670.0,
            form: StockForm::RoundBar {
                diameter_mm: 28.0,
                length_mm: 18.0,
            },
            gears: vec![ext_gear(2.0, 24, 16.0, 8)],
        },
        // Sun ×1 — Ø22×20 bar, bore 8 (Swiss); external Z18 (illustrative).
        Component {
            name: "sun",
            per_box_count: 1,
            qty: 100,
            bbox: [22.0, 22.0, 20.0],
            volume_mm3: 6_597.0,
            form: StockForm::RoundBar {
                diameter_mm: 22.0,
                length_mm: 20.0,
            },
            gears: vec![ext_gear(2.0, 18, 18.0, 8)],
        },
        // Carrier ×1 — prismatic 90×90×15 block (3-axis); no gears. The
        // control: every gap is inert ⇒ upgraded == naive.
        Component {
            name: "carrier",
            per_box_count: 1,
            qty: 100,
            bbox: [90.0, 90.0, 15.0],
            volume_mm3: 60_000.0,
            form: StockForm::RectangularBlock,
            gears: vec![],
        },
        // Pin ×5 — Ø8×22 bar (Swiss lights-out); near-net, no gears.
        Component {
            name: "pin",
            per_box_count: 5,
            qty: 500,
            bbox: [8.0, 8.0, 22.0],
            volume_mm3: 1_105.0,
            form: StockForm::RoundBar {
                diameter_mm: 8.0,
                length_mm: 22.0,
            },
            gears: vec![],
        },
        // Output hub ×1 — Ø40×25 bar, bore 20 (OD 40 > 32 ⇒ turn-mill); no gears.
        Component {
            name: "hub",
            per_box_count: 1,
            qty: 100,
            bbox: [40.0, 40.0, 25.0],
            volume_mm3: 25_000.0,
            form: StockForm::RoundBar {
                diameter_mm: 40.0,
                length_mm: 25.0,
            },
            gears: vec![],
        },
    ]
}

const GLOBAL_RATE: f64 = 1.6667;
/// The external €444/box figure Ervin ran (ADR-0094 Context / Q4). Not in this
/// tree; used only as the documented direction target.
const EXTERNAL_BASELINE_EUR: f64 = 444.0;
/// The legacy flat manual gear adder the pre-Gap-3 workflow tacked on per box
/// (ADR-0094 Context). Makes the naive in-tree baseline an apples-to-apples
/// "old way" quote for a gearbox.
const LEGACY_FLAT_GEAR_ADDER_EUR: f64 = 95.0;

// ── PINNED GOLDEN (4 dp), recorded 2026-06-25 ──────────────────────────────
// Per-component upgraded total_price (€/part):
const PIN_RING: f64 = 227.3354;
const PIN_PLANET: f64 = 10.2000;
const PIN_SUN: f64 = 10.6314;
const PIN_CARRIER: f64 = 80.2016;
const PIN_PIN: f64 = 1.0492;
const PIN_HUB: f64 = 11.4491;
// Per-box rollups (Σ part_total × per-box count):
const PIN_BOX_UPGRADED: f64 = 385.8635;
const PIN_BOX_NAIVE: f64 = 308.7331;

#[test]
fn planetary_box_pins_per_component_and_per_box_totals() {
    let comps = the_box();
    let mut box_up = 0.0;
    let mut box_naive = 0.0;
    for c in &comps {
        let up = c.upgraded();
        let nv = c.naive();
        box_up += up.total_price * c.per_box_count as f64;
        box_naive += nv.total_price * c.per_box_count as f64;
        let pin = match c.name {
            "ring" => PIN_RING,
            "planet" => PIN_PLANET,
            "sun" => PIN_SUN,
            "carrier" => PIN_CARRIER,
            "pin" => PIN_PIN,
            "hub" => PIN_HUB,
            other => panic!("unexpected component {other}"),
        };
        assert_eq!(
            round4(up.total_price),
            pin,
            "{} upgraded total_price drifted from pinned golden",
            c.name
        );
        assert!(up.total_price.is_finite() && up.total_price > 0.0);
    }
    assert_eq!(
        round4(box_up),
        PIN_BOX_UPGRADED,
        "per-box upgraded € drifted"
    );
    assert_eq!(round4(box_naive), PIN_BOX_NAIVE, "per-box naive € drifted");
}

#[test]
fn assertion_1_every_cost_line_present_and_decomposable() {
    for c in &the_box() {
        let bd = c.upgraded();
        let log = bd.reasoning_log.join("\n");
        let has = |needle: &str| {
            assert!(
                bd.reasoning_log.iter().any(|l| l.contains(needle)),
                "{}: reasoning_log missing `{}`\n--- log ---\n{}",
                c.name,
                needle,
                log
            )
        };
        // Material → machining → setup → cad_cam → subtotal → overhead →
        // margin → total → margin-floor gate: every line is named & inspectable.
        has("base_material_cost=");
        has("machining_cost=");
        has("setup_cost =");
        has("cad_cam_cost = hours=");
        has("= subtotal=");
        has("overhead = subtotal * overhead_factor=");
        has("margin = (subtotal + overhead)");
        has("total_price = subtotal + overhead + margin");
        has("min_margin floor");
        // Decomposability: the named lines reconstruct the subtotal & total.
        let recon_sub =
            bd.material_cost + bd.machining_cost + bd.setup_cost + bd.cad_cam_cost + bd.gear_cost;
        let recon_total = recon_sub + bd.overhead + bd.margin;
        assert!(
            (recon_total - bd.total_price).abs() < 1e-9,
            "{}: lines must reconstruct total",
            c.name
        );
        if c.gears.is_empty() {
            assert_eq!(bd.gear_cost, 0.0, "{}: no gears ⇒ gear_cost 0", c.name);
            assert!(
                !bd.reasoning_log.iter().any(|l| l.contains("[gear")),
                "{}: no gear lines",
                c.name
            );
        } else {
            has("[gear");
            has("total gear_cost=");
            has("+ gear=");
        }
    }
}

#[test]
fn assertion_2_turned_parts_route_lights_out_not_flat_rate() {
    // Planets / sun / pins (OD ≤ bar cap 32) ⇒ Swiss-turn-mill lights-out 0.525.
    for name in ["planet", "sun", "pin"] {
        let c = the_box().into_iter().find(|c| c.name == name).unwrap();
        let bd = c.upgraded();
        assert!(
            bd.reasoning_log
                .iter()
                .any(|l| l.contains("routed_family=swiss-turn-mill")
                    && l.contains("lights_out_eligible=true")
                    && l.contains("effective_rate=0.5250")),
            "{}: must route to Swiss lights-out 0.5250 €/min (not flat 1.6667)",
            name
        );
    }
    // Ring + hub (OD > bar cap) ⇒ turn-mill lights-out 0.720 — still far below flat.
    for name in ["ring", "hub"] {
        let c = the_box().into_iter().find(|c| c.name == name).unwrap();
        let bd = c.upgraded();
        assert!(
            bd.reasoning_log
                .iter()
                .any(|l| l.contains("routed_family=turn-mill")
                    && l.contains("lights_out_eligible=true")
                    && l.contains("effective_rate=0.7200")),
            "{}: must route to turn-mill lights-out 0.7200 €/min",
            name
        );
    }
    // Carrier (prismatic) ⇒ 3-axis attended = the flat €100/h rate, no lights-out.
    let carrier = the_box().into_iter().find(|c| c.name == "carrier").unwrap();
    assert!(carrier.upgraded().reasoning_log.iter().any(|l| l
        .contains("routed_family=3-axis-mill")
        && l.contains("lights_out_eligible=false")));
    // And the lights-out effective €/min is strictly below the flat global,
    // derived from the breakdown itself (machining_cost / billable_minutes;
    // Standard tolerance ⇒ mult 1.0, no thin-wall/quote_multiplier bumps here).
    for name in ["planet", "hub"] {
        let c = the_box().into_iter().find(|c| c.name == name).unwrap();
        let bd = c.upgraded();
        let billable = bd.machining_minutes + bd.inspection_minutes;
        let effective = bd.machining_cost / billable;
        assert!(
            effective < GLOBAL_RATE,
            "{name}: lights-out effective {effective:.4} €/min must be below flat {GLOBAL_RATE}"
        );
    }
}

#[test]
fn assertion_3_gears_costed_external_cheaper_than_internal() {
    // gear_cost > 0 on geared parts, == 0 on ungeared.
    for c in &the_box() {
        let g = c.upgraded().gear_cost;
        if c.gears.is_empty() {
            assert_eq!(g, 0.0, "{}: ungeared ⇒ gear_cost 0", c.name);
        } else {
            assert!(g > 0.0, "{}: geared ⇒ gear_cost > 0 (got {})", c.name, g);
        }
    }
    // External skiving ≪ internal ring shaping/EDM, isolated on the ring blank
    // (Ø100 bar ⇒ turn-mill): one external Z50 vs one internal Z60, same blank.
    let ring = the_box().into_iter().find(|c| c.name == "ring").unwrap();
    let probe = |gears: Vec<GearOp>| -> f64 {
        let mut fg = ring.upgraded_graph();
        fg.gears = gears;
        quote_with_shop_model(
            &fg,
            &[aluminium_6061()],
            &catchall_complexity_rules(),
            &default_tolerance_multipliers(),
            &no_stock_adjustments(),
            &params(),
            ring.qty,
            ToleranceRange::Standard,
            &CalibrationTable::neutral(),
            &machine_rates(),
            &gear_rates(),
        )
        .unwrap()
        .gear_cost
    };
    let external_only = probe(vec![ext_gear(2.0, 50, 18.0, 8)]);
    let internal_only = probe(vec![int_gear(2.0, 60, 18.0, 8)]);
    assert!(external_only > 0.0 && internal_only > 0.0);
    assert!(
        external_only < internal_only,
        "external skive ({external_only}) must be far cheaper than internal ring shape ({internal_only})"
    );
    // The geared parts auto-selected the right processes — surfaced in the log.
    let ring_log = ring.upgraded().reasoning_log;
    assert!(ring_log.iter().any(|l| l.contains("selected power_skive")));
    assert!(ring_log.iter().any(|l| l.contains("selected shape")));
}

#[test]
fn assertion_4_round_parts_bill_less_material_than_bbox_block() {
    // Gap 1: a round bar occupies π/4 ≈ 78.5 % of its bounding box, so every
    // turned part bills LESS material than the same prismatic block would.
    for c in &the_box() {
        let up = c.upgraded();
        let nv = c.naive();
        match c.form {
            StockForm::RectangularBlock => {
                // Carrier control: same block ⇒ identical material.
                assert_eq!(
                    round4(up.material_cost),
                    round4(nv.material_cost),
                    "{}",
                    c.name
                );
            }
            _ => {
                assert!(
                    up.material_cost < nv.material_cost,
                    "{}: round material {} must be < bbox-block material {}",
                    c.name,
                    up.material_cost,
                    nv.material_cost
                );
                // π/4 ratio (pre stock-adjust/tax, which are off here): exact.
                assert!(
                    (up.material_cost / nv.material_cost - std::f64::consts::FRAC_PI_4).abs()
                        < 1e-6
                );
            }
        }
    }
}

#[test]
fn assertion_5_box_total_finite_positive_and_beats_realistic_baseline() {
    let comps = the_box();
    let mut box_up = 0.0;
    let mut box_naive = 0.0;
    let mut nongear_up = 0.0;
    let mut nongear_naive = 0.0;
    for c in &comps {
        let up = c.upgraded().total_price * c.per_box_count as f64;
        let nv = c.naive().total_price * c.per_box_count as f64;
        box_up += up;
        box_naive += nv;
        if c.gears.is_empty() && !matches!(c.form, StockForm::RectangularBlock) {
            // pins + hub: gaps 1+2 only, purely cost-reducing.
            nongear_up += up;
            nongear_naive += nv;
        }
    }

    // (a) finite & positive.
    assert!(box_up.is_finite() && box_up > 0.0);

    // (b) below the €444 external baseline (the documented direction target).
    assert!(
        box_up < EXTERNAL_BASELINE_EUR,
        "per-box upgraded {box_up:.4} must be below the €{EXTERNAL_BASELINE_EUR} external baseline"
    );

    // (c) below the realistic pre-gap quote: naive block baseline + the legacy
    //     flat gear adder the old workflow actually used for a gearbox.
    let naive_pregap = box_naive + LEGACY_FLAT_GEAR_ADDER_EUR;
    assert!(
        box_up < naive_pregap,
        "per-box upgraded {box_up:.4} must beat the realistic pre-gap quote {naive_pregap:.4} \
         (naive block {box_naive:.4} + legacy €{LEGACY_FLAT_GEAR_ADDER_EUR} gear adder)"
    );

    // (d) the non-geared turned parts (pins + hub) are STRICTLY cheaper
    //     upgraded — gaps 1+2 unambiguously moved the number down.
    assert!(
        nongear_up < nongear_naive,
        "non-gear turned parts upgraded {nongear_up:.4} must be < naive {nongear_naive:.4}"
    );

    // (e) HONEST FLAG (documented, not hidden): against the STRICT no-gear
    //     baseline the upgraded box is HIGHER, because Gap 3 surfaces real,
    //     previously-invisible gear cost. This is correct behaviour for a
    //     gearbox; the no-gear baseline under-quotes it.
    assert!(
        box_naive < box_up,
        "strict no-gear baseline {box_naive:.4} is expected BELOW upgraded {box_up:.4} \
         (Gap 3 adds the gear cost the naive model omits) — see file header FLAG"
    );
}

#[test]
fn dump_decomposition_for_report() {
    // Always-passing dump; run with `--nocapture` to read the per-line € the
    // engine produced (used to author the report + verify the pinned goldens).
    let comps = the_box();
    let mut box_up = 0.0;
    let mut box_naive = 0.0;
    eprintln!("\n=== Ø100 planetary box — per-box € (upgraded engine vs naive baseline) ===");
    eprintln!(
        "{:8} {:>3} {:>4} {:14} {:>7} {:>8} {:>8} {:>9} {:>10} | {:>10}",
        "comp", "cnt", "qty", "routed", "mat", "mach", "gear", "tot/pt", "x count", "naive/pt"
    );
    for c in &comps {
        let up = c.upgraded();
        let nv = c.naive();
        box_up += up.total_price * c.per_box_count as f64;
        box_naive += nv.total_price * c.per_box_count as f64;
        let routed = if up
            .reasoning_log
            .iter()
            .any(|l| l.contains("swiss-turn-mill"))
        {
            "swiss-turn-mill"
        } else if up
            .reasoning_log
            .iter()
            .any(|l| l.contains("routed_family=turn-mill"))
        {
            "turn-mill"
        } else {
            "3-axis-mill"
        };
        eprintln!(
            "{:8} {:>3} {:>4} {:14} {:>7.3} {:>8.3} {:>8.3} {:>9.4} {:>10.4} | {:>10.4}",
            c.name,
            c.per_box_count,
            c.qty,
            routed,
            up.material_cost,
            up.machining_cost,
            up.gear_cost,
            up.total_price,
            up.total_price * c.per_box_count as f64,
            nv.total_price,
        );
    }
    eprintln!("--------------------------------------------------------------------------------");
    eprintln!(
        "PER-BOX upgraded = {:.4}   naive(no-gear,block,3-axis) = {:.4}   naive+legacy€95 = {:.4}   external = {:.1}",
        box_up,
        box_naive,
        box_naive + LEGACY_FLAT_GEAR_ADDER_EUR,
        EXTERNAL_BASELINE_EUR
    );
}
