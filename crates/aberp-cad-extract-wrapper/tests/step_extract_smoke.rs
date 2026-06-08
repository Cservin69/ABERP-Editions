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

use aberp_cad_extract_wrapper::{CadExtractor, ExtractRequest, EXPECTED_SCHEMA_VERSION};

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

    assert_eq!(graph.schema_version, EXPECTED_SCHEMA_VERSION);
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
