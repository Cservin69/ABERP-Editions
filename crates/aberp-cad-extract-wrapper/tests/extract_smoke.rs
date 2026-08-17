//! End-to-end smoke test: synthesize a 20 mm cube STL on disk, spawn
//! the real Python `aberp-cad-extract` CLI, deserialize the output
//! through the wrapper, and pin the shape of the result.
//!
//! Requires a Python interpreter with `aberp_cad_extract` installed.
//! Locally: `python3 -m venv .venv-cad-extract && .venv-cad-extract/bin/pip
//! install -e python/aberp-cad-extract`, then either source the venv or
//! `export ABERP_TEST_PYTHON=$PWD/.venv-cad-extract/bin/python`.
//!
//! The test asserts the geometry the Python CLI test pins
//! (`test_cli_emits_valid_feature_graph_json`) — same fixture geometry,
//! same expected output. If the two diverge, the wire contract has
//! drifted and BOTH sides update in the same diff.

use std::time::Duration;

use aberp_cad_extract_wrapper::{
    CadExtractor, ExtractRequest, EXPECTED_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};

mod common;
use common::{test_python_bin, write_cube_stl};

#[test]
fn cube_stl_extracts_into_feature_graph_via_real_python() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let stl = tmp.path().join("cube.stl");
    write_cube_stl(&stl, 20.0).expect("write cube STL");

    let extractor = CadExtractor::new()
        .with_python_bin(test_python_bin())
        .with_timeout(Duration::from_secs(15));

    let req = ExtractRequest {
        input_path: stl,
        material_grade: "6061-T6".to_string(),
    };

    let graph = match extractor.extract(&req) {
        Ok(g) => g,
        Err(e) => {
            panic!("smoke test failed: {e}\n(install the Python extractor in the test interpreter)")
        }
    };

    assert!(
        (MIN_SCHEMA_VERSION..=EXPECTED_SCHEMA_VERSION).contains(&graph.schema_version),
        "extractor emitted v{}, outside the accepted range",
        graph.schema_version
    );
    assert_eq!(graph.bounding_box_mm, [20.0, 20.0, 20.0]);
    assert_eq!(graph.material_grade, "6061-T6");
    // STL v1 returns an empty features list — that's honest, per the
    // Python-side stl extractor docstring ("STL is a triangle-soup
    // format with no semantic feature data").
    assert!(
        graph.features.is_empty(),
        "v1 STL extractor returns empty features list: {:?}",
        graph.features
    );
    // Addendum-1 booleans MUST be present and typed bool — the
    // serde struct refuses missing or null. Both should be false
    // for a 20 mm solid cube (no thin walls, no 5-axis features).
    assert!(!graph.requires_5_axis);
    assert!(!graph.thin_wall_present);
    // Volume of a 20 mm cube is 8 000 mm³. STL signed-tetrahedra
    // sum is exact for axis-aligned cubes; allow 0.01 mm³ slop for
    // f32→f64 rounding.
    assert!(
        (graph.volume_mm3 - 8_000.0).abs() < 0.01,
        "cube volume should be ~8 000 mm³, got {}",
        graph.volume_mm3
    );
}
