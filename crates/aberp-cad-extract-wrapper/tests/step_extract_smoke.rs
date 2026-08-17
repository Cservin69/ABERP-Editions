//! End-to-end STEP smoke test (PR-273): point the wrapper at the
//! committed `unit_cube.step` fixture under
//! `python/aberp-cad-extract/aberp_cad_extract/tests/fixtures/`,
//! spawn the real Python CLI through it, and pin the shape of the
//! resulting [`FeatureGraph`].
//!
//! Requires a Python interpreter with `aberp_cad_extract` installed
//! AND the `[step]` extra (cadquery-ocp + vtk + proxy). The CI lane
//! sets `ABERP_TEST_PYTHON` to a venv created with
//!   `pip install -e '.[step,dev]'`.
//!
//! The expected geometry matches the Python-side fixture: a 20 mm
//! axis-aligned cube → bounding box [20, 20, 20], volume 8000 mm³,
//! neither addendum-1 boolean tripped (solid cube fills its bbox; no
//! thin walls).

use std::path::PathBuf;
use std::time::Duration;

use aberp_cad_extract_wrapper::{
    CadExtractor, ExtractRequest, EXPECTED_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};

mod common;
use common::test_python_bin;

fn step_fixture_path(name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/aberp-cad-extract-wrapper.
    // Walk up two levels to repo root, then into the Python fixtures.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.parent()
        .and_then(|p| p.parent())
        .expect("repo root above the crate directory")
        .join("python/aberp-cad-extract/aberp_cad_extract/tests/fixtures")
        .join(name)
}

#[test]
fn step_cube_extracts_into_feature_graph_via_real_python() {
    let fixture = step_fixture_path("unit_cube.step");
    assert!(
        fixture.exists(),
        "STEP fixture missing at {}: regenerate via the PR-273 helper",
        fixture.display()
    );

    let extractor = CadExtractor::new()
        .with_python_bin(test_python_bin())
        .with_timeout(Duration::from_secs(15));

    let req = ExtractRequest {
        input_path: fixture,
        material_grade: "6061-T6".to_string(),
    };

    let graph = match extractor.extract(&req) {
        Ok(g) => g,
        Err(e) => panic!(
            "STEP smoke failed: {e}\n\
             (install the Python extractor with `pip install -e '.[step]'` \
             in the test interpreter — OCP wheel is ~63 MB)"
        ),
    };

    // ADR-0112 B.1: the guard is a RANGE, so the smoke test asserts the
    // range too. Pinning exact equality here would re-create, in a test,
    // exactly the lockstep coupling the range guard exists to remove.
    assert!(
        (MIN_SCHEMA_VERSION..=EXPECTED_SCHEMA_VERSION).contains(&graph.schema_version),
        "extractor emitted v{}, outside the accepted {}..={}",
        graph.schema_version,
        MIN_SCHEMA_VERSION,
        EXPECTED_SCHEMA_VERSION
    );
    // AddOptimal_s gives an exact 20.0 bbox for an axis-aligned cube;
    // serde converts through f64 with no precision loss.
    assert_eq!(graph.bounding_box_mm, [20.0, 20.0, 20.0]);
    assert_eq!(graph.material_grade, "6061-T6");
    // STEP v1 also returns empty features — BREP feature mining is a
    // follow-on cut, same posture as the STL extractor.
    assert!(
        graph.features.is_empty(),
        "v1 STEP extractor returns empty features list: {:?}",
        graph.features
    );
    // Addendum 1: both booleans present, typed bool, and false for a
    // solid cube (fills bbox completely; no thin walls).
    assert!(!graph.requires_5_axis);
    assert!(!graph.thin_wall_present);
    // OCCT VolumeProperties is exact within float tolerance for a
    // primitive Box; allow 0.01 mm³ slop.
    assert!(
        (graph.volume_mm3 - 8_000.0).abs() < 0.01,
        "cube volume should be ~8 000 mm³, got {}",
        graph.volume_mm3
    );
}

/// ADR-0112 Part B, end-to-end through the REAL wire.
///
/// The Python-side pins (`test_holes.py`) prove the miner; this proves
/// the whole path — OCCT face-walk -> Pydantic model -> canonical JSON ->
/// subprocess stdout -> serde -> the Rust `LocatedHole` struct — with no
/// field lost, renamed or mistyped in between. A cross-language wire
/// contract is exactly the kind of thing that compiles fine on both sides
/// while agreeing on nothing.
#[test]
fn step_plate_yields_four_located_holes_through_the_real_wire() {
    use aberp_cad_extract_wrapper::FeatureGraph;

    let fixture = step_fixture_path("plate_4_through_holes.step");
    assert!(
        fixture.exists(),
        "STEP fixture missing at {}: regenerate via \
         python/aberp-cad-extract/tools/generate_step_fixtures.py",
        fixture.display()
    );

    let extractor = CadExtractor::new()
        .with_python_bin(test_python_bin())
        .with_timeout(Duration::from_secs(30));

    let graph: FeatureGraph = extractor
        .extract(&ExtractRequest {
            input_path: fixture,
            material_grade: "6061-T6".to_string(),
        })
        .unwrap_or_else(|e| {
            panic!(
                "located-hole smoke failed: {e}\n\
                 (install the Python extractor with `pip install -e '.[step]'`)"
            )
        });

    // The extractor emits v6 now; the range guard is what makes that safe.
    assert!(
        (MIN_SCHEMA_VERSION..=EXPECTED_SCHEMA_VERSION).contains(&graph.schema_version),
        "extractor emitted v{}, outside the accepted range",
        graph.schema_version
    );

    assert_eq!(
        graph.located_holes.len(),
        4,
        "the 100x60x12 plate has four Ø8 through-holes; got {:?}",
        graph.located_holes
    );

    // Exact geometry — same numbers the fixture was BUILT from.
    let expected_entries = [
        [20.0, 20.0, 0.0],
        [20.0, 40.0, 0.0],
        [80.0, 20.0, 0.0],
        [80.0, 40.0, 0.0],
    ];
    for (hole, entry) in graph.located_holes.iter().zip(expected_entries) {
        assert!((hole.diameter_mm - 8.0).abs() < 1e-6, "diameter: {hole:?}");
        assert!((hole.depth_mm - 12.0).abs() < 1e-6, "depth: {hole:?}");
        for (got, want) in hole.axis_unit.iter().zip([0.0, 0.0, 1.0]) {
            assert!((got - want).abs() < 1e-6, "axis: {hole:?}");
        }
        for (got, want) in hole.entry_point_mm.iter().zip(entry) {
            assert!((got - want).abs() < 1e-6, "entry: {hole:?}");
        }
        assert_eq!(
            hole.end_condition,
            aberp_quote_engine::HoleEndCondition::Through
        );
        assert!(!hole.flat_bottom);
    }
}

/// A hole-less part must come back over the wire with the field EMPTY,
/// and its canonical encoding must carry no `located_holes` key at all.
///
/// CORRECTED (ADR-0112 adversarial S1). This used to end "…which is what
/// keeps its `feature_graph_hash` unchanged", and that was false on
/// exactly the path this test exercises — the REAL extractor. The same
/// cut moved the Python `SCHEMA_VERSION` from 2 to 6, `_schema_version`
/// sits inside the encoding the daemon blake3-hashes, so this cube's
/// hash DOES change. What the absent key buys is that `located_holes`
/// contributes nothing to that change — worth having, asserted below,
/// and not the same thing as hash stability. Claiming the stronger
/// property is what stopped anyone checking the weaker one.
#[test]
fn step_cube_yields_no_located_holes() {
    let extractor = CadExtractor::new()
        .with_python_bin(test_python_bin())
        .with_timeout(Duration::from_secs(30));

    let graph = extractor
        .extract(&ExtractRequest {
            input_path: step_fixture_path("unit_cube.step"),
            material_grade: "6061-T6".to_string(),
        })
        .expect("cube extraction");

    assert!(
        graph.located_holes.is_empty(),
        "a solid cube has no cylindrical faces: {:?}",
        graph.located_holes
    );
    // Re-encoding it emits no key — this field adds no bytes.
    let out = serde_json::to_string(&graph).expect("serialize");
    assert!(!out.contains("located_holes"), "{out}");
    // …and the version field is the one that DID move, stated here so
    // the encoding change is visible at the site that used to deny it.
    assert!(
        out.contains(r#""_schema_version":6"#),
        "the extractor stamps v6, which is the (deliberate, one-time) \
         reason this part's feature_graph_hash differs from its pre-v6 \
         value: {out}"
    );
}
