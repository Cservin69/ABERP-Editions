//! S8 / ADR-0094 — **REAL-PART validation golden**: GearSppiners60, a real
//! ~Ø60 × 14 mm machined gear/fidget spinner that Ervin uploaded to ABERP's
//! portal. The portal's **frozen-prod** engine (v0.0.0, schema_v2 — the model
//! BEFORE the S1–S6 gap-closure) returned an indicative **€102.42/part** at
//! qty 100. This fixture re-quotes the SAME real part through the UPGRADED
//! editions engine ([`quote_with_shop_model`]) and pins a line-by-line
//! BEFORE/AFTER decomposition, attributing every euro of movement to one of
//! the three closed gaps.
//!
//! ## The real part (ABERP extractor geometry, independently confirmed by
//!    Dispatch's STL parse — identical)
//!
//! * bbox **59.70 × 59.98 × 14.00 mm**; finished volume **23 641 mm³**;
//!   surface area **14 671 mm²**; **0 features** (STL = triangle soup, no
//!   topology); material **316 stainless** (density 8.0 g/cm³,
//!   machining_difficulty 2.2, €6/kg, NO stock adjustment); qty **100**;
//!   **Standard** tolerance, not 5-axis.
//!
//! ## BEFORE — the frozen-prod €102.42 quote, reproduced byte-for-byte
//!
//! The editions engine is a strict SUPERSET of frozen prod: the legacy
//! [`quote`] entry point (rectangular block, no machine rates, no gears) is
//! inert-by-default and reproduces the exact frozen-prod decomposition —
//! material €2.7672 · machining €58.6240 (35.1737 min @ flat €1.6667/min =
//! 9.3527 roughing [34.01 cm³ removed × diff 2.2 / MRR 8] + 25.8210 finishing
//! [146.71 cm² × 0.08 × 2.2]) · setup €0.3333 · CAD-CAM €1.5000 · gear €0
//! (teeth invisible to the pre-Gap-3 model) · overhead €12.6449 (20 %) ·
//! margin €26.5543 (35 %) · **total €102.4238**. [`old_reproduces_frozen_prod`]
//! pins this — proof the upgrade did not silently move the legacy path.
//!
//! ## AFTER — the upgraded quote, and the three levers
//!
//! Re-quoted via [`quote_with_shop_model`] with the seeded shop (6 machine
//! families, 5 gear processes) the part is a turned Ø60 disc, so:
//!
//! 1. **StockForm::RoundBar OD 60 × L 14** — the shop buys Ø60 bar, not a
//!    59.70×59.98×14 block. Round form_volume π/4·60²·14 = 39 584 mm³ vs block
//!    50 131 mm³ ⇒ **~21 % less material billed** AND less roughing
//!    removed-volume (21.88 cm³ vs 34.01 cm³). Lever 1 lowers material +
//!    roughing.
//! 2. **Machine-family routing → TurnMill, lights-out** — OD 60 > bar_capacity
//!    32 ⇒ routes to `turn-mill` (not Swiss). The seeded TurnMill rate
//!    (attended €1.60/min, lights_out_factor 0.45, unattended_capable) makes
//!    the unattended job bill at **effective €0.7200/min** instead of the flat
//!    €1.6667/min. Lever 2 is the dominant cost reducer.
//! 3. **Gear-generation op** — it is a GEAR spinner; the old engine costed the
//!    teeth at €0. One representative external gear (illustrative — see FLAG)
//!    auto-routes to in-cycle **PowerSkive** on the turn-mill, adding a real,
//!    decomposed **gear_cost €5.6016** the old quote omitted. Lever 3 raises
//!    the price — correctly.
//!
//! Net: **€102.4238 → €52.7204/part** (−€49.70, −48.5 %). The drop is
//! dominated by Lever 2 (lights-out routing, −€48.83), with Lever 1 (round
//! bar, −€9.95) reducing material+roughing and Lever 3 (gear, +€9.07)
//! correctly ADDING the previously-invisible tooth-generation cost. The new
//! number is more ACCURATE on all three axes; it lands lower for the right
//! reasons, NOT because inputs were rigged. [`upgraded_pins_new_decomposition`]
//! + [`lever_waterfall_attributes_each_delta`] pin and attribute every euro.
//!
//! ## ⚠️ ILLUSTRATIVE GEAR DATA — RECHECK WITH REAL SHOP/DRAWING DATA
//!
//! The STL is a triangle soup: 0 features, no tooth topology. The single
//! external gear below (module 1.5, **Z = 36**, face 14, AGMA 8) is an
//! illustrative reconstruction chosen to be internally consistent with the
//! ~Ø58 toothed rim of a Ø60 part — it is NOT measured from a drawing. The
//! seed machine rates and gear-process coefficients are the same ADR-0094 Gap
//! 2/3 illustrative seeds as the S7 planetary fixture; they self-correct via
//! the S429 calibration loop once Ervin enters real shop €/min and a real gear
//! drawing. **Before quoting GearSppiners60 for real, recheck the tooth count,
//! module, face width, AGMA class, the €/min, and the lights-out factor.** The
//! magnitude of the −€49.70 swing (esp. Lever 2) is seed-dependent; the
//! DIRECTION (turned disc, unattended turn-mill, teeth now billed) is sound.
//!
//! ## ⚠️ LEAD-TIME 415-DAY FLAG — a CONFIG red flag, not a price issue
//!
//! The frozen-prod portal also quoted a **415 calendar-day** lead time. That
//! is the capacity model's empty-machine fallback ([`lead_time_days`] with no
//! `quoting_machines` rows → a single virtual shop at
//! [`FALLBACK_DAILY_HOURS`]×(1−[`FALLBACK_BUFFER_PCT`]/100) = 12.8 h/day
//! clearing the whole backlog). It is independent of the pricing function and
//! is fixed by SEEDING `quoting_machines` capacities, not by changing a price.
//! [`lead_time_415_is_empty_machine_fallback`] demonstrates the fallback
//! signature (we cannot reproduce the exact 415 without prod's live backlog
//! hours, and we do not pretend to).
//!
//! ## Pinned golden (4 dp), recorded 2026-06-27
//!
//! Engine `quote_with_shop_model`, qty 100, Standard tolerance, neutral
//! calibration, REAL surface area (14 671 mm², not the bbox fallback). Numbers
//! are locked to 4 dp and cross-checked against an independent in-test
//! reference implementation of the engine arithmetic ([`reference_total`]).

mod common;

use aberp_quote_engine::{
    lead_time_days, quote, quote_with_shop_model, CalibrationTable, Feature, FeatureGraph,
    GearKind, GearOp, GearProcess, GearProcessRate, MachineCapacity, MachineFamily, MachineRate,
    Material, QuoteBreakdown, QuotingParameters, StockForm, StockStatus, ToleranceRange,
    FALLBACK_BUFFER_PCT, FALLBACK_DAILY_HOURS,
};
use common::{catchall_complexity_rules, default_tolerance_multipliers, no_stock_adjustments};
use std::collections::BTreeMap;

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

// ── The real part's geometry (ABERP extractor; Dispatch-confirmed). ────────
const BBOX: [f64; 3] = [59.70, 59.98, 14.00];
const PART_VOLUME_MM3: f64 = 23_641.0;
const PART_SURFACE_AREA_MM2: f64 = 14_671.0;
const QTY: u32 = 100;
// The bought Ø60 bar (max envelope 59.98 ⇒ next standard bar is Ø60) × 14 thk.
const BAR_OD_MM: f64 = 60.0;
const BAR_LEN_MM: f64 = 14.0;

// ── 316 stainless (brief): density 8.0, difficulty 2.2, €6/kg, in-stock,
//    NO stock adjustment, NOT exotic (no inconel/titanium substring). ───────
fn ss_316() -> Material {
    Material {
        grade: "316".to_string(),
        density_g_cm3: 8.0,
        cost_per_kg_eur: 6.0,
        machining_difficulty: 2.2,
        quote_multiplier: 1.0,
        stock_status: StockStatus::InStock,
    }
}

// ── S418 day-1 params (report §8.1): the exact frozen-prod knobs. ──────────
fn params() -> QuotingParameters {
    QuotingParameters {
        scrap_factor: 0.15,
        profit_margin_base: 0.35,
        overhead_factor: 0.20,
        setup_amortization_threshold: 5,
        min_margin: 0.10,
        exotic_material_tax: 0.05,
        machining_rate_eur_per_minute: 1.6667, // €100/machine-hour (the flat rate)
        cad_cam_rate_eur_per_hour: 100.0,
        cad_cam_base_hours: 1.0,
        mrr_rough_ref_cm3_per_min: 8.0,
        t_finish_min_per_cm2: 0.08,
        setup_base_min: 20.0,
        setup_5axis_min: 25.0,
        bar_capacity_mm: 32.0,
    }
}

// ── Seeded shop — identical illustrative seeds to the S7 planetary fixture
//    (ADR-0094 Gap 2 proposed €/min). Turn-mill: attended €1.60, lights-out
//    factor 0.45, unattended-capable ⇒ effective €0.72/min. ──────────────────
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

// ── Seeded gear processes — identical illustrative seeds to S7. PowerSkive
//    in-cycle factor 0.5 (the part is already on the turn-mill spindle). ─────
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

// ── The single representative external gear (ILLUSTRATIVE — see file FLAG).
//    module 1.5, Z 36, face 14, AGMA 8, Auto ⇒ engine selects PowerSkive on
//    the turn-mill. Z 36 @ m1.5 ⇒ pitch Ø54, OD ≈ 57 — consistent with the
//    ~Ø58 toothed rim of the Ø60 part. RECHECK against a real drawing. ───────
fn spinner_gear() -> GearOp {
    GearOp {
        kind: GearKind::ExternalSpurHelical,
        module_mm: 1.5,
        teeth: 36,
        face_width_mm: 14.0,
        quality_agma: 8,
        process: GearProcess::Auto,
    }
}

/// BEFORE graph: rectangular block (the bbox), NO gears — exactly what the
/// frozen-prod schema_v2 extractor produced for this STL.
fn old_graph() -> FeatureGraph {
    FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        bounding_box_mm: BBOX,
        volume_mm3: PART_VOLUME_MM3,
        surface_area_mm2: PART_SURFACE_AREA_MM2,
        material_grade: "316".to_string(),
        features: Vec::<Feature>::new(),
        requires_5_axis: false,
        thin_wall_present: false,
        stock_form: StockForm::RectangularBlock,
        gears: Vec::new(),
    }
}

/// BEFORE quote = frozen-prod legacy path: global flat rate, block, no gears.
fn old_quote() -> QuoteBreakdown {
    quote(
        &old_graph(),
        &[ss_316()],
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &params(),
        QTY,
        ToleranceRange::Standard,
    )
    .expect("frozen-prod reproduction quote must succeed")
}

/// Full shop-model quote for an arbitrary (form, gears) — the staging helper
/// behind the lever waterfall.
fn shop_quote(form: StockForm, gears: Vec<GearOp>, rates: &[MachineRate]) -> QuoteBreakdown {
    let mut fg = old_graph();
    fg.stock_form = form;
    fg.gears = gears;
    quote_with_shop_model(
        &fg,
        &[ss_316()],
        &catchall_complexity_rules(),
        &default_tolerance_multipliers(),
        &no_stock_adjustments(),
        &params(),
        QTY,
        ToleranceRange::Standard,
        &CalibrationTable::neutral(),
        rates,
        &gear_rates(),
    )
    .expect("shop-model quote must succeed")
}

/// AFTER quote: round bar + gear + full seeded shop.
fn new_quote() -> QuoteBreakdown {
    shop_quote(
        StockForm::RoundBar {
            diameter_mm: BAR_OD_MM,
            length_mm: BAR_LEN_MM,
        },
        vec![spinner_gear()],
        &machine_rates(),
    )
}

const BLOCK: StockForm = StockForm::RectangularBlock;
const ROUND: StockForm = StockForm::RoundBar {
    diameter_mm: BAR_OD_MM,
    length_mm: BAR_LEN_MM,
};

// ════════════════ PINNED GOLDEN (4 dp), recorded 2026-06-27 ════════════════
// BEFORE — frozen-prod €102.42, reproduced:
const OLD_MATERIAL: f64 = 2.7672;
const OLD_MACHINING: f64 = 58.6240;
const OLD_SETUP: f64 = 0.3333;
const OLD_CADCAM: f64 = 1.5000;
const OLD_GEAR: f64 = 0.0;
const OLD_OVERHEAD: f64 = 12.6449;
const OLD_MARGIN: f64 = 26.5543;
const OLD_TOTAL: f64 = 102.4238;
const OLD_MACHINING_MIN: f64 = 35.1737;

// AFTER — upgraded engine:
const NEW_MATERIAL: f64 = 2.1850;
const NEW_MACHINING: f64 = 22.9235;
const NEW_GEAR: f64 = 5.6016;
const NEW_SETUP: f64 = 0.3333;
const NEW_CADCAM: f64 = 1.5000;
const NEW_OVERHEAD: f64 = 6.5087;
const NEW_MARGIN: f64 = 13.6682;
const NEW_TOTAL: f64 = 52.7204;
const NEW_MACHINING_MIN: f64 = 31.8381;
const NEW_EFFECTIVE_RATE: f64 = 0.7200;

// Lever waterfall — per-part total at each stage:
const STAGE0_TOTAL: f64 = 102.4238; // block / flat / no gear  (= frozen prod)
const STAGE1_TOTAL: f64 = 92.4745; //  + round bar (flat, no gear)
const STAGE2_TOTAL: f64 = 43.6458; //  + lights-out turn-mill routing (no gear)
const STAGE3_TOTAL: f64 = 52.7204; //  + gear costing  (= NEW)

/// Independent reference: recompute a total from first principles, mirroring
/// the engine arithmetic, for an (effective €/min, form_volume, gear_cost)
/// triple. Cross-checks the engine output without re-using its code path.
fn reference_total(effective_rate: f64, form_volume_mm3: f64, gear_cost: f64) -> f64 {
    let p = params();
    let bbox_volume = BBOX[0] * BBOX[1] * BBOX[2];
    let stock = form_volume_mm3 * (1.0 + p.scrap_factor);
    let material = stock * 8.0 / 1_000_000.0 * 6.0; // density 8, €6/kg, no adj/tax
    let removed_cm3 = (stock - PART_VOLUME_MM3).max(0.0) / 1000.0;
    let roughing = removed_cm3 * 2.2 / p.mrr_rough_ref_cm3_per_min;
    let finishing = PART_SURFACE_AREA_MM2 / 100.0 * p.t_finish_min_per_cm2 * 2.2;
    let machining = (roughing + finishing) * effective_rate; // standard tol mult 1.0
    let setup = p.setup_base_min * p.machining_rate_eur_per_minute / QTY as f64;
    let fill = PART_VOLUME_MM3 / bbox_volume; // 0.4716 ⇒ med-fill ⇒ base+0.5h
    let cad_h = p.cad_cam_base_hours
        + if (0.30..0.60).contains(&fill) {
            0.5
        } else {
            0.0
        };
    let cadcam = cad_h * p.cad_cam_rate_eur_per_hour / QTY as f64;
    let subtotal = material + machining + setup + cadcam + gear_cost;
    let overhead = subtotal * p.overhead_factor;
    let margin = (subtotal + overhead) * p.profit_margin_base;
    subtotal + overhead + margin
}

#[test]
fn old_reproduces_frozen_prod() {
    // The editions engine's legacy `quote()` path reproduces the frozen-prod
    // €102.42 decomposition byte-for-byte — proof the upgrade left the legacy
    // (block / flat / no-gear) path untouched (inert-by-default superset).
    let bd = old_quote();
    assert_eq!(round4(bd.material_cost), OLD_MATERIAL, "material");
    assert_eq!(round4(bd.machining_cost), OLD_MACHINING, "machining");
    assert_eq!(round4(bd.setup_cost), OLD_SETUP, "setup");
    assert_eq!(round4(bd.cad_cam_cost), OLD_CADCAM, "cad_cam");
    assert_eq!(
        bd.gear_cost, OLD_GEAR,
        "gear (pre-Gap-3 model is teeth-blind)"
    );
    assert_eq!(round4(bd.overhead), OLD_OVERHEAD, "overhead");
    assert_eq!(round4(bd.margin), OLD_MARGIN, "margin");
    assert_eq!(round4(bd.total_price), OLD_TOTAL, "total");
    assert_eq!(
        round4(bd.machining_minutes),
        OLD_MACHINING_MIN,
        "mach minutes"
    );
    // No machine-rate line, no gear line on the legacy path.
    assert!(
        !bd.reasoning_log
            .iter()
            .any(|l| l.contains("machine-rate row")),
        "legacy path must not emit a machine-rate line"
    );
    assert!(
        !bd.reasoning_log.iter().any(|l| l.contains("[gear")),
        "legacy path must not emit a gear line"
    );
    // Independent cross-check: flat rate, block volume, zero gear.
    let bbox_volume = BBOX[0] * BBOX[1] * BBOX[2];
    assert!((bd.total_price - reference_total(1.6667, bbox_volume, 0.0)).abs() < 1e-9);
}

#[test]
fn upgraded_pins_new_decomposition() {
    let bd = new_quote();
    assert_eq!(round4(bd.material_cost), NEW_MATERIAL, "material");
    assert_eq!(round4(bd.machining_cost), NEW_MACHINING, "machining");
    assert_eq!(round4(bd.gear_cost), NEW_GEAR, "gear");
    assert_eq!(round4(bd.setup_cost), NEW_SETUP, "setup");
    assert_eq!(round4(bd.cad_cam_cost), NEW_CADCAM, "cad_cam");
    assert_eq!(round4(bd.overhead), NEW_OVERHEAD, "overhead");
    assert_eq!(round4(bd.margin), NEW_MARGIN, "margin");
    assert_eq!(round4(bd.total_price), NEW_TOTAL, "total");
    assert_eq!(
        round4(bd.machining_minutes),
        NEW_MACHINING_MIN,
        "mach minutes"
    );
    // Lines reconstruct the subtotal & total exactly (full decomposability).
    let recon_sub =
        bd.material_cost + bd.machining_cost + bd.setup_cost + bd.cad_cam_cost + bd.gear_cost;
    assert!((recon_sub + bd.overhead + bd.margin - bd.total_price).abs() < 1e-9);
    // Independent cross-check: turn-mill lights-out €0.72/min, round-bar
    // volume, the gear cost the engine reported.
    let rb_vol = std::f64::consts::FRAC_PI_4 * BAR_OD_MM * BAR_OD_MM * BAR_LEN_MM;
    assert!((bd.total_price - reference_total(0.72, rb_vol, bd.gear_cost)).abs() < 1e-9);
    assert!(bd.total_price.is_finite() && bd.total_price > 0.0);
}

#[test]
fn lever_waterfall_attributes_each_delta() {
    // Stage the three levers one at a time; each stage's per-part total isolates
    // exactly one lever's contribution. (overhead+margin are flat %, so
    // total = subtotal × 1.20 × 1.35 = subtotal × 1.62 at every stage.)
    let s0 = old_quote().total_price; // block / flat / no gear  (frozen prod)
    let s1 = shop_quote(ROUND, vec![], &[]).total_price; // + round bar (empty rates ⇒ flat)
    let s2 = shop_quote(ROUND, vec![], &machine_rates()).total_price; // + lights-out routing
    let s3 = new_quote().total_price; // + gear costing  (= NEW)

    assert_eq!(round4(s0), STAGE0_TOTAL, "stage0 (frozen prod)");
    assert_eq!(round4(s1), STAGE1_TOTAL, "stage1 (+round bar)");
    assert_eq!(round4(s2), STAGE2_TOTAL, "stage2 (+lights-out)");
    assert_eq!(round4(s3), STAGE3_TOTAL, "stage3 (+gear = NEW)");

    // Lever directions, honestly: 1 & 2 DOWN, 3 UP, net DOWN.
    let l1 = s1 - s0; // round bar: material + roughing
    let l2 = s2 - s1; // lights-out routing: machining €/min
    let l3 = s3 - s2; // gear costing: teeth now billed
    assert!(l1 < 0.0, "L1 round-bar must reduce total (got {l1:+.4})");
    assert!(l2 < 0.0, "L2 lights-out must reduce total (got {l2:+.4})");
    assert!(l3 > 0.0, "L3 gear costing must RAISE total (got {l3:+.4})");
    // Lever 2 (lights-out routing) is the dominant mover.
    assert!(
        l2.abs() > l1.abs() && l2.abs() > l3.abs(),
        "lights-out routing must dominate the swing (l1={l1:+.4} l2={l2:+.4} l3={l3:+.4})"
    );
    // Net: roughly halved, but for the right reasons (not rigged cheaper).
    let net = s3 - s0;
    assert!((net - (NEW_TOTAL - OLD_TOTAL)).abs() < 1e-3);
    assert!(net < 0.0 && s3 < s0, "net must be DOWN: {s0:.4} -> {s3:.4}");
}

#[test]
fn lever1_roundbar_bills_less_material_and_roughs_less() {
    // Gap 1: the Ø60 bar occupies π/4·60²·14 = 39 584 mm³ vs the 50 131 mm³
    // block ⇒ less material billed AND less roughing removed-volume.
    let block = shop_quote(BLOCK, vec![], &[]);
    let round = shop_quote(ROUND, vec![], &[]); // empty rates ⇒ same flat rate, isolates Gap 1
    assert!(
        round.material_cost < block.material_cost,
        "round material {:.4} must be < block material {:.4}",
        round.material_cost,
        block.material_cost
    );
    // Ratio = (π/4·60²·14)/(59.70·59.98·14) ≈ 0.7896 — NOT exactly π/4, because
    // the bought Ø60 bar is slightly oversize vs the 59.70×59.98 envelope.
    let ratio = round.material_cost / block.material_cost;
    assert!((ratio - 0.7896).abs() < 1e-3, "material ratio {ratio:.4}");
    // Less stock ⇒ fewer roughing minutes ⇒ fewer total machining minutes
    // (finishing is area-driven and unchanged).
    assert!(
        round.machining_minutes < block.machining_minutes,
        "round mach min {:.4} must be < block {:.4}",
        round.machining_minutes,
        block.machining_minutes
    );
    assert_eq!(round4(round.machining_minutes), NEW_MACHINING_MIN);
    assert!(round
        .reasoning_log
        .iter()
        .any(|l| l.contains("stock_form=round_bar")));
}

#[test]
fn lever2_routes_turnmill_lights_out_not_flat() {
    // Gap 2: OD 60 > bar_capacity 32 ⇒ turn-mill (NOT Swiss); unattended-
    // capable + bar stock + qty ≥ amortization ⇒ lights-out €0.7200/min.
    let bd = shop_quote(ROUND, vec![], &machine_rates());
    assert!(
        bd.reasoning_log
            .iter()
            .any(|l| l.contains("routed_family=turn-mill")
                && l.contains("lights_out_eligible=true")
                && l.contains("effective_rate=0.7200")),
        "must route to turn-mill lights-out 0.7200 €/min (not flat 1.6667)\n{}",
        bd.reasoning_log.join("\n")
    );
    // Effective €/min derived from the breakdown itself (Standard tol ⇒ mult
    // 1.0, no thin-wall/quote_multiplier here) is far below the flat rate.
    let billable = bd.machining_minutes + bd.inspection_minutes;
    let effective = bd.machining_cost / billable;
    assert!(
        (effective - NEW_EFFECTIVE_RATE).abs() < 1e-4,
        "eff {effective:.4}"
    );
    assert!(
        effective < 1.6667,
        "lights-out {effective:.4} must beat flat 1.6667"
    );
}

#[test]
fn lever3_gear_costed_powerskive_in_cycle() {
    // Gap 3: the old engine costed teeth at €0; the new engine bills a real,
    // decomposed gear op. External + turn-mill ⇒ auto PowerSkive, in-cycle.
    let old = old_quote();
    let new = new_quote();
    assert_eq!(old.gear_cost, 0.0, "pre-Gap-3 model omits teeth entirely");
    assert!(new.gear_cost > 0.0, "geared part ⇒ gear_cost > 0");
    assert_eq!(round4(new.gear_cost), NEW_GEAR, "gear cost pin");

    let log = new.reasoning_log.join("\n");
    assert!(
        log.contains("selected power_skive"),
        "auto-selects PowerSkive"
    );
    assert!(
        log.contains("in-cycle on turn-mill"),
        "skived in-cycle on turn-mill"
    );
    assert!(
        log.contains("total gear_cost=5.6016"),
        "gear total surfaced in log"
    );
    assert!(
        log.contains("+ gear=5.6016"),
        "gear folded into the subtotal line"
    );

    // Independent gear-math cross-check (PowerSkive seed, in-cycle on turn-mill
    // @ €0.72/min): setup 8 + z36·0.10·1.5^1·(14/10)·(1+0·0.10) = 8 + 7.56 =
    // 15.56 → ×0.5 in-cycle = 7.78 min → ×0.72 = €5.6016.
    let gen_min = 36.0 * 0.10 * 1.5_f64.powf(1.0) * (14.0 / 10.0) * 1.0;
    let expected_gear = (8.0 + gen_min) * 0.5 * 0.72;
    assert!(
        (new.gear_cost - expected_gear).abs() < 1e-9,
        "gear math {expected_gear:.6}"
    );
}

#[test]
fn lead_time_415_is_empty_machine_fallback() {
    // The 415-day lead time the portal quoted is NOT a price issue — it is the
    // capacity model's empty-`quoting_machines` fallback. With no machines
    // seeded the model collapses every family onto one virtual shop at
    // 16 h/day × (1 − 20 %) = 12.8 h/day and flags `used_fallback`.
    let cap = FALLBACK_DAILY_HOURS * (1.0 - FALLBACK_BUFFER_PCT / 100.0);
    assert_eq!(cap, 12.8);

    // Empty machines ⇒ fallback signature (the 415-day number's provenance).
    // We model only this run's hours; the real 415 also folds prod's live
    // backlog, which is not in this tree — so we assert the FALLBACK PATH, not
    // the exact 415 (documented, not faked).
    let this_run: BTreeMap<MachineFamily, f64> =
        [(MachineFamily::TurnMill, 66.0)].into_iter().collect();
    let est = lead_time_days(&[], &BTreeMap::new(), &this_run);
    assert!(
        est.used_fallback,
        "no machines ⇒ virtual-shop fallback (the 415-day cause)"
    );
    assert_eq!(est.binding_family, None, "fallback has no binding family");

    // The fix is a CONFIG seed, not a price change: enter one real turn-mill
    // and the fallback is gone (binding family becomes real, no flag).
    let machines = [MachineCapacity {
        family: MachineFamily::TurnMill,
        daily_hours_avail: 20.0,
        buffer_pct: 15.0,
    }];
    let seeded = lead_time_days(&machines, &BTreeMap::new(), &this_run);
    assert!(
        !seeded.used_fallback,
        "seeding quoting_machines clears the fallback flag"
    );
    assert_eq!(seeded.binding_family, Some(MachineFamily::TurnMill));
}

#[test]
fn dump_before_after_for_report() {
    // Always-passing dump; `cargo test -- --nocapture` prints the BEFORE/AFTER
    // table used to author the report and verify the pins.
    let old = old_quote();
    let new = new_quote();
    let s1 = shop_quote(ROUND, vec![], &[]).total_price;
    let s2 = shop_quote(ROUND, vec![], &machine_rates()).total_price;
    eprintln!("\n=== GearSppiners60 — BEFORE (frozen prod v0.0.0) vs AFTER (editions) — €/part, qty 100 ===");
    eprintln!(
        "{:12} {:>12} {:>12} {:>12}",
        "line", "BEFORE", "AFTER", "delta"
    );
    let row = |n: &str, a: f64, b: f64| eprintln!("{n:12} {a:>12.4} {b:>12.4} {:>12.4}", b - a);
    row("material", old.material_cost, new.material_cost);
    row("machining", old.machining_cost, new.machining_cost);
    row("gear", old.gear_cost, new.gear_cost);
    row("setup", old.setup_cost, new.setup_cost);
    row("cad_cam", old.cad_cam_cost, new.cad_cam_cost);
    row("overhead", old.overhead, new.overhead);
    row("margin", old.margin, new.margin);
    row("TOTAL", old.total_price, new.total_price);
    eprintln!(
        "\nwaterfall: frozen {:.4} --L1_roundbar {:+.4}--> {:.4} --L2_lightsout {:+.4}--> {:.4} --L3_gear {:+.4}--> {:.4}",
        old.total_price,
        s1 - old.total_price,
        s1,
        s2 - s1,
        s2,
        new.total_price - s2,
        new.total_price
    );
    eprintln!(
        "NET {:+.4} ({:+.1}%)  — material+machining DOWN (round bar + lights-out), gear UP (teeth now billed)",
        new.total_price - old.total_price,
        (new.total_price / old.total_price - 1.0) * 100.0
    );
}
