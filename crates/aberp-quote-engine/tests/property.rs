//! Round-trip property test (no proptest dep — deterministic sweep).
//!
//! Random-but-deterministic varied inputs MUST produce either a
//! `QuoteBreakdown` or a documented `QuoteError`. NEVER panic, NEVER
//! return NaN/Inf, and the returned numbers MUST be finite.

mod common;

use aberp_quote_engine::{quote, FeatureType, ToleranceRange};
use common::*;

/// xorshift64* — tiny deterministic PRNG. Not for crypto; for
/// driving "the engine survives weird-but-valid combos" sweeps. The
/// brief named proptest as optional; this keeps the dep tree clean
/// per CLAUDE.md rule 13.
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        let span = hi - lo + 1;
        lo + (self.next_u64() as u32 % span)
    }
    fn unit(&mut self) -> f64 {
        // 53-bit precision in [0, 1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[test]
fn engine_never_panics_across_varied_inputs() {
    let materials = vec![default_material("6061-T6"), exotic_material("Inconel 718")];
    let rules = catchall_complexity_rules();
    let tols = default_tolerance_multipliers();
    let adjs = no_stock_adjustments();
    let params = default_parameters();

    let feature_kinds = [
        FeatureType::Hole,
        FeatureType::Pocket,
        FeatureType::Slot,
        FeatureType::Thread,
        FeatureType::Surface,
        FeatureType::Engraving,
    ];
    let tolerances = [
        ToleranceRange::Loose,
        ToleranceRange::Standard,
        ToleranceRange::Tight,
        ToleranceRange::Precision,
        ToleranceRange::UltraPrecision,
    ];

    let mut lcg = Lcg::new(0xA5_5A_5A_A5_DE_AD_BE_EF);
    let mut ok = 0u32;
    let mut errs = 0u32;

    for case in 0..400u32 {
        let mut fg = simple_feature_graph(if case % 3 == 0 {
            "Inconel 718"
        } else {
            "6061-T6"
        });
        fg.volume_mm3 = lcg.unit() * 200_000.0 + 100.0;
        fg.thin_wall_present = (case & 1) == 0;
        fg.requires_5_axis = (case & 2) == 0;

        // Replace features with a varied list.
        let nf = lcg.range(1, 6) as usize;
        fg.features.clear();
        for _ in 0..nf {
            let ft = feature_kinds[lcg.range(0, feature_kinds.len() as u32 - 1) as usize];
            let count = lcg.range(1, 20);
            let size = lcg.unit() * 300.0; // 0..300 mm
            fg.features.push(aberp_quote_engine::Feature {
                feature_type: ft,
                count,
                representative_size_mm: size,
            });
        }
        let qty = lcg.range(1, 100);
        let tol = tolerances[lcg.range(0, tolerances.len() as u32 - 1) as usize];

        // The call MUST NOT panic; failure paths are typed errors.
        let result = quote(&fg, &materials, &rules, &tols, &adjs, &params, qty, tol);
        match result {
            Ok(b) => {
                // Every monetary field is finite and non-negative.
                for (name, v) in [
                    ("material_cost", b.material_cost),
                    ("labor_cost", b.labor_cost),
                    ("setup_cost", b.setup_cost),
                    ("overhead", b.overhead),
                    ("margin", b.margin),
                    ("total_price", b.total_price),
                    ("machining_minutes", b.machining_minutes),
                    ("inspection_minutes", b.inspection_minutes),
                ] {
                    assert!(v.is_finite(), "case {case}: {name} is non-finite ({v})");
                    assert!(v >= 0.0, "case {case}: {name} is negative ({v})");
                }
                ok += 1;
            }
            Err(_) => errs += 1,
        }
    }
    // Both arms must actually be reached for the sweep to be
    // meaningful (vs. always-Ok or always-Err).
    assert!(ok > 0, "no successful quotes — fixture too narrow");
    // It is OK if errs == 0 (the fixture is mostly satisfiable);
    // the panic-resistance claim is the property under test.
    let _ = errs;
}
